use crate::render::{mouse_in_history_box, mouse_in_scrollbar_column};
use crate::state::{
    App, INPUT_PAD, InputBuffer, PAGE_SCROLL_LINES, Page, find_turn_at_row, input_inner_width,
};
use crate::{ShellCommand, clipboard, parse_input_line, selection};
use choreo_client_core::{
    ClientError, broken_pipe, build_add_credential_message, resolve_private_key, shell_command_echo,
};
use choreo_proto::ClientMessage;
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};

pub(super) fn handle_chat_event(
    event: Event,
    app: &mut App,
    client_tx: &std::sync::mpsc::Sender<ClientMessage>,
) -> Result<(), ClientError> {
    match event {
        Event::Key(key) => {
            if key.kind != KeyEventKind::Press {
                return Ok(());
            }
            // Any keypress clears transient status/error messages.
            app.status = None;
            app.error = None;
            // Don't clear help on Ctrl+H itself — let the toggle arm handle it.
            if key.code != KeyCode::Char('h') || !key.modifiers.contains(KeyModifiers::CONTROL) {
                app.show_ctrl_help = false;
            }
            match key.code {
                // All Ctrl+ combinations delegated to a dedicated handler.
                _ if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    handle_chat_ctrl_key(key, app, client_tx)?;
                }
                // Alt+Enter → continue generation
                KeyCode::Enter if key.modifiers.contains(KeyModifiers::ALT) => {
                    if app.attached_session_id.is_some() {
                        tracing::debug!("Alt+Enter continuing generation");
                        let request_id = app.next_request_id;
                        app.next_request_id = app.next_request_id.wrapping_add(1);
                        // The guard above already checked attached_session_id,
                        // but the display entry may not exist yet — track the
                        // in-flight request only when there is a display to
                        // record it in, never panic on a missing one.
                        if let Some(display) = app.active_display() {
                            display.active.insert(request_id);
                        }
                        client_tx
                            .send(ClientMessage::ContinueGeneration { request_id })
                            .map_err(broken_pipe)?;
                        app.scroll_to(0);
                    } else {
                        tracing::debug!("Alt+Enter ignored — no session attached");
                        app.status = Some("no session attached".to_string());
                    }
                }
                KeyCode::Esc => {
                    if app.attached_session_id.is_some() {
                        tracing::debug!("Esc stopping generation");
                        client_tx
                            .send(ClientMessage::Cancel { request_id: 0 })
                            .map_err(broken_pipe)?;
                    } else {
                        tracing::debug!("Esc ignored — no session attached");
                        app.status = Some("no session attached".to_string());
                    }
                }
                KeyCode::Up => {
                    let inner = app
                        .last_terminal_size
                        .map(|(w, _)| input_inner_width(w))
                        .unwrap_or(78);
                    if app.input.is_on_first_visual_line(inner) {
                        app.navigate_history_up();
                    } else {
                        app.input.cursor_up(inner);
                        app.ensure_input_cursor_visible();
                    }
                }
                KeyCode::Down => {
                    let inner = app
                        .last_terminal_size
                        .map(|(w, _)| input_inner_width(w))
                        .unwrap_or(78);
                    if app.input.is_on_last_visual_line(inner) {
                        // Down only drives history navigation while an entry
                        // is loaded.  When editing the draft itself there is
                        // nothing below it, so Down on the last visual line
                        // lands at end-of-line instead of being a dead key —
                        // the mirror of Up recalling history from the first
                        // line.
                        if app.history_index.is_some() {
                            app.navigate_history_down();
                        } else {
                            app.input.cursor_end_line();
                            app.ensure_input_cursor_visible();
                        }
                    } else {
                        app.input.cursor_down(inner);
                        app.ensure_input_cursor_visible();
                    }
                }
                KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                    app.input.insert_char_at_cursor('\n');
                    app.ensure_input_cursor_visible();
                }
                KeyCode::Enter => {
                    let line = app.input.text.trim().to_string();
                    app.input.clear();
                    // The prompt was sent — forget the per-session draft so it
                    // doesn't resurface when the user returns to this session.
                    app.clear_current_draft();
                    app.commit_to_history();
                    match parse_input_line(&line, &mut app.next_request_id, app.attached_session_id)
                    {
                        ShellCommand::Empty => {}
                        ShellCommand::InvalidCancel(value) => {
                            app.status = Some(format!("invalid request id: {value}"))
                        }
                        ShellCommand::UnknownCommand(error) => app.status = Some(error),
                        ShellCommand::Send(message) => {
                            // Client-side validation: reject reasoning slugs that
                            // the attached model's capability set does not include.
                            // This provides faster feedback than waiting for the
                            // daemon to reply with ReasoningEffortSetFailed.
                            if let ClientMessage::SetReasoningEffort { ref effort } = message
                                && effort != "off"
                            {
                                let valid = app
                                    .active_display_ref()
                                    .and_then(|d| d.reasoning_capability.as_ref())
                                    .map(|c| c.available_effort_levels.iter().any(|l| l == effort))
                                    .unwrap_or(true); // No capability cached → let daemon validate
                                if !valid {
                                    tracing::warn!(
                                        %effort,
                                        "TUI rejected reasoning slug not in capability set",
                                    );
                                    app.status = Some(format!(
                                        "model does not support reasoning '{effort}'"
                                    ));
                                    return Ok(());
                                }
                            }
                            let message = match message {
                                ClientMessage::CreateSession {
                                    title,
                                    parent_session_id,
                                    working_dir,
                                    context_config,
                                    account_name,
                                    selected_model,
                                    reasoning_effort,
                                } => ClientMessage::CreateSession {
                                    title,
                                    parent_session_id,
                                    // Inherit fields from the currently attached session
                                    // when not explicitly provided by the user.
                                    working_dir: working_dir.or_else(|| {
                                        app.active_display_ref().and_then(|d| d.working_dir.clone())
                                    }),
                                    context_config,
                                    account_name: account_name.or_else(|| {
                                        app.active_display_ref()
                                            .and_then(|d| d.account_name.clone())
                                    }),
                                    selected_model: selected_model.or_else(|| {
                                        app.active_display_ref()
                                            .and_then(|d| d.selected_model.clone())
                                    }),
                                    reasoning_effort: reasoning_effort.or_else(|| {
                                        app.active_display_ref()
                                            .and_then(|d| d.reasoning_effort.clone())
                                    }),
                                },
                                other => other,
                            };
                            if let Some(echo) =
                                shell_command_echo(&ShellCommand::Send(message.clone()))
                            {
                                app.status = Some(echo);
                            }
                            if let ClientMessage::RunInput { request_id, .. } = &message {
                                app.error = None;
                                // The active display tracks in-flight request ids
                                // for the spinner; with no session active there is
                                // nothing to track, so skip instead of panicking.
                                if let Some(display) = app.active_display() {
                                    display.active.insert(*request_id);
                                }
                            }
                            client_tx.send(message).map_err(broken_pipe)?;

                            // Scroll the history view to the bottom so the user can
                            // see their submitted message appear as the daemon
                            // processes it.  Without this, a user who has scrolled
                            // up to read past conversation would remain scrolled up
                            // and miss the new content arriving at the bottom.
                            app.scroll_to(0);
                        }
                        ShellCommand::Unlock { method } => match resolve_private_key(&method) {
                            Ok(private_key) => {
                                let _ = client_tx.send(ClientMessage::Unlock { private_key });
                            }
                            Err(e) => {
                                tracing::warn!("[choreo-tui] unlock failed: {e}");
                            }
                        },
                        ShellCommand::AddCredential {
                            ref service,
                            ref credential_type,
                            ref fields,
                            unlock,
                        } => {
                            match build_add_credential_message(
                                service.clone(),
                                credential_type.clone(),
                                fields.clone(),
                                unlock,
                            ) {
                                Ok(msg) => {
                                    let _ = client_tx.send(msg);
                                }
                                Err(e) => {
                                    tracing::warn!("[choreo-tui] add credential failed: {e}");
                                }
                            }
                        }
                        ShellCommand::RemoveCredential { ref service } => {
                            if let Some(echo) =
                                shell_command_echo(&ShellCommand::RemoveCredential {
                                    service: service.clone(),
                                })
                            {
                                app.status = Some(echo);
                            }
                            let _ = client_tx.send(ClientMessage::RemoveCredential {
                                service: service.clone(),
                            });
                        }
                        ShellCommand::Undo => {
                            if let Some(echo) = shell_command_echo(&ShellCommand::Undo) {
                                app.status = Some(echo);
                            }
                            let _ = client_tx.send(ClientMessage::Undo);
                        }
                        ShellCommand::Redo => {
                            if let Some(echo) = shell_command_echo(&ShellCommand::Redo) {
                                app.status = Some(echo);
                            }
                            let _ = client_tx.send(ClientMessage::Redo);
                        }
                        ShellCommand::Continue => {
                            if let Some(echo) = shell_command_echo(&ShellCommand::Continue) {
                                app.status = Some(echo);
                            }
                            if app.attached_session_id.is_some() {
                                let request_id = app.next_request_id;
                                app.next_request_id = app.next_request_id.wrapping_add(1);
                                // The guard above already checked attached_session_id,
                                // but the display entry may not exist yet — only
                                // track the request when there is a display to hold it.
                                if let Some(display) = app.active_display() {
                                    display.active.insert(request_id);
                                }
                                client_tx
                                    .send(ClientMessage::ContinueGeneration { request_id })
                                    .map_err(broken_pipe)?;
                                app.scroll_to(0);
                            } else {
                                app.status = Some("no session attached".to_string());
                            }
                        }
                        ShellCommand::Stop => {
                            if let Some(echo) = shell_command_echo(&ShellCommand::Stop) {
                                app.status = Some(echo);
                            }
                            // Send Cancel with request_id 0 (the CANCEL_ALL sentinel)
                            // to stop whatever request is currently active on the
                            // attached session and all its children.
                            if app.attached_session_id.is_some() {
                                client_tx
                                    .send(ClientMessage::Cancel { request_id: 0 })
                                    .map_err(broken_pipe)?;
                            } else {
                                app.status = Some("no session attached".to_string());
                            }
                        }
                        ShellCommand::RefreshModels { force } => {
                            // Show immediate feedback; the daemon replies
                            // asynchronously via ModelsRefreshed /
                            // ModelsRefreshFailed.
                            let suffix = if force { " (forced)" } else { "" };
                            app.status = Some(format!("refreshing models…{suffix}"));
                            client_tx
                                .send(ClientMessage::RefreshModels { force })
                                .map_err(broken_pipe)?;
                        }
                    }
                }
                KeyCode::Backspace
                | KeyCode::Delete
                | KeyCode::Left
                | KeyCode::Right
                | KeyCode::Home
                | KeyCode::End => {
                    handle_input_key(key, &mut app.input);
                    app.ensure_input_cursor_visible();
                }
                KeyCode::Char(_) => {
                    handle_input_key(key, &mut app.input);
                    app.ensure_input_cursor_visible();
                }
                KeyCode::PageUp => {
                    app.scroll_up(PAGE_SCROLL_LINES);
                }
                KeyCode::PageDown => {
                    app.scroll_down(PAGE_SCROLL_LINES);
                }
                _ => {}
            }
        }
        // Text selection in the history pane: while a selection gesture is in
        // progress (mouse-down in the history box through mouse-up), every
        // mouse event extends or finalizes it.  Checked before the scrollbar
        // arms so a drag that crosses the scrollbar column keeps selecting —
        // matching terminal-native selection, where a drag spans the whole
        // screen.  A scrollbar click can never arm a text selection (its Down
        // lands in the scrollbar arm below, which never calls
        // `start_selection`).  The gesture state machine lives in `selection`;
        // only the clipboard write and the status message belong to the UI
        // loop.
        Event::Mouse(mouse) if selection::is_selecting(app) => {
            if let Some(text) = selection::handle_selection_mouse(app, &mouse) {
                if clipboard::copy_to_clipboard(&text) {
                    tracing::info!(
                        bytes = text.len(),
                        "[choreo-tui] copied selection to clipboard via OSC 52"
                    );
                    app.status = Some("Selection copied to clipboard.".to_string());
                } else {
                    // Over the OSC 52 size cap: say so instead of
                    // pretending the copy succeeded (see clipboard.rs).
                    tracing::warn!(
                        bytes = text.len(),
                        "[choreo-tui] selection exceeds the OSC 52 size cap; not copied"
                    );
                    app.status = Some("Selection too large to copy to clipboard.".to_string());
                }
            }
        }
        // Left-click (and drag) in the scrollbar column.
        // This must be checked BEFORE the drag handler so that a new click
        // on the scrollbar always reaches this handler, even when the drag
        // flag is still set from a previous click.  Only treated as a
        // scrollbar when one is actually rendered: on sessions whose history
        // fits the viewport the column is blank, and a click there must not
        // arm the drag state (which would swallow the next history click).
        Event::Mouse(mouse)
            if app.scrollbar_visible()
                && mouse_in_scrollbar_column(
                    mouse.column,
                    mouse.row,
                    app.history_viewport.width,
                    app.history_viewport.height,
                ) =>
        {
            match mouse.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    app.scrollbar_dragging = true;

                    // Check whether the click lands on a user-text marker.
                    let top_slot = 2 * mouse.row as usize;
                    let bot_slot = top_slot + 1;

                    let marker_hit = app.active_display_ref().and_then(|d| {
                        d.markers
                            .iter()
                            .find(|m| m.virtual_slot == top_slot || m.virtual_slot == bot_slot)
                    });

                    if let Some(marker) = marker_hit {
                        app.scroll_to_content_line(marker.content_line);
                    } else {
                        app.scroll_to_track_row(mouse.row, app.history_viewport.height);
                    }
                }
                MouseEventKind::Drag(MouseButton::Left) => {
                    app.scroll_to_track_row(mouse.row, app.history_viewport.height);
                }
                MouseEventKind::ScrollUp => {
                    app.scrollbar_scroll_up();
                }
                MouseEventKind::ScrollDown => {
                    app.scrollbar_scroll_down();
                }
                _ => {}
            }
        }
        // While the user is dragging the scrollbar thumb, route all
        // mouse events through the drag handler regardless of whether
        // the cursor is inside or outside the narrow scrollbar column.
        // This arm catches drags that have exited the scrollbar column.
        Event::Mouse(mouse) if app.scrollbar_dragging => {
            match mouse.kind {
                MouseEventKind::Drag(MouseButton::Left) => {
                    app.scroll_to_track_row(mouse.row, app.history_viewport.height);
                }
                MouseEventKind::Up(MouseButton::Left) => {
                    app.scrollbar_dragging = false;
                }
                _ => {
                    // Any other mouse event (scroll, right-click, etc.)
                    // cancels the drag.
                    app.scrollbar_dragging = false;
                }
            }
        }
        Event::Mouse(mouse)
            if mouse_in_history_box(
                mouse.column,
                mouse.row,
                app.history_viewport.width,
                app.history_viewport.height,
            ) =>
        {
            // Accumulate scroll events rather than scrolling immediately.
            // All accumulated deltas are applied in a single batch each
            // frame by `apply_scroll_delta`, which reads the accumulator
            // and resets it to zero — this prevents per-event re-renders
            // and ensures no momentum carries between frames.
            match mouse.kind {
                MouseEventKind::ScrollUp => {
                    app.scroll_accumulator = app.scroll_accumulator.saturating_add(1);
                }
                MouseEventKind::ScrollDown => {
                    app.scroll_accumulator = app.scroll_accumulator.saturating_sub(1);
                }
                // Left-click on an image opens it fullscreen.  Uses
                // `TurnImageLayout` (populated by `rebuild_height_prefix`) to
                // map the click's content-line offset within the turn to
                // the correct image index — no text-height recomputation
                // or cache dependency needed.
                MouseEventKind::Down(MouseButton::Left) => {
                    // Resolve the clicked (turn, offset) once and share it
                    // across the three hit-tests below (reasoning header,
                    // tool-result header, image) instead of re-running the
                    // row→turn binary search per target.
                    if let Some((turn_idx, offset)) = find_turn_at_row(app, mouse.row) {
                        // A click on the reasoning header row toggles the
                        // collapsible reasoning section.  Checked before the
                        // other targets so the header wins when they overlap.
                        let reasoning_toggle = app
                            .active_display_ref()
                            .and_then(|d| d.turn_layouts.get(turn_idx))
                            .and_then(|l| l.reasoning_header_range)
                            .filter(|&(start, end)| offset >= start && offset < end)
                            .and_then(|_| {
                                app.active_display_ref()
                                    .and_then(|d| d.visible_turn_ids.get(turn_idx))
                                    .copied()
                            });
                        // A click on a tool result's header row toggles that
                        // result's collapsible body.  The range index maps
                        // directly onto `turn.tool_results`, whose `call_id`
                        // keys the per-result collapse override.  Checked
                        // before image hit-testing, after the reasoning header.
                        let tool_toggle = app
                            .active_display_ref()
                            .and_then(|d| d.turn_layouts.get(turn_idx))
                            .and_then(|l| {
                                l.tool_result_header_ranges
                                    .iter()
                                    .position(|&(start, end)| offset >= start && offset < end)
                            })
                            .and_then(|range_idx| {
                                let display = app.active_display_ref()?;
                                let turn_id = display.visible_turn_ids.get(turn_idx).copied()?;
                                let turn = display.view.turns.get(&turn_id)?;
                                let call_id = turn.tool_results.get(range_idx)?.call_id.clone();
                                Some((turn_id, call_id))
                            });
                        if let Some(turn_id) = reasoning_toggle {
                            if let Some(display) = app.active_display() {
                                display.toggle_reasoning(turn_id);
                            }
                        } else if let Some((turn_id, call_id)) = tool_toggle {
                            if let Some(display) = app.active_display() {
                                display.toggle_tool_result(turn_id, &call_id);
                            }
                        } else if let Some(layout) = app
                            .active_display_ref()
                            .and_then(|d| d.turn_layouts.get(turn_idx))
                            && let Some(img_idx) = layout
                                .image_ranges
                                .iter()
                                .position(|&(start, end)| offset >= start && offset < end)
                            && let Some(turn_id) = app
                                .active_display_ref()
                                .and_then(|d| d.visible_turn_ids.get(turn_idx))
                                .copied()
                            && let Some(session_id) = app.active_session_id
                        {
                            app.fullscreen_image_target = Some((session_id, turn_id, img_idx));
                        } else {
                            // Plain-text click: arm a potential text selection
                            // at the click point.  It only becomes real
                            // (highlighted + copied on release) once the drag
                            // moves; a plain click keeps its existing behavior
                            // (none here — the toggle/fullscreen targets above
                            // are the only interactive rows).  Skipped for
                            // those targets so a click that toggles a
                            // reasoning/tool header or opens an image never
                            // leaves a dangling armed selection behind.
                            selection::start_selection(app, mouse.row, mouse.column);
                        }
                    }
                }
                _ => {}
            }
        }
        // Left-click inside the command input box repositions the text cursor.
        // The box rect is computed with the same layout math as the renderer
        // (`App::input_box_rect`), so a click lands on exactly the cell that
        // was drawn.  Clicks on the box's top/bottom borders are ignored;
        // clicks in the left/right padding clamp to the first/last column of
        // the line.  Scrollbar and history-box clicks are handled by the arms
        // above, whose regions never overlap this box.
        Event::Mouse(mouse) if mouse.kind == MouseEventKind::Down(MouseButton::Left) => {
            if let Some((term_w, term_h)) = app.last_terminal_size {
                let box_rect = app.input_box_rect(term_w, term_h);
                // Use the box's own width (not the terminal width) so the
                // wrap width matches the renderer's `input_inner_width`, and
                // mirror the renderer's drawn content height (box height minus
                // its two borders) so the click's visual window is clamped
                // exactly like the drawn one.
                let inner_width = input_inner_width(box_rect.width);
                let visible_height = (box_rect.height.saturating_sub(2)) as usize;
                // Row must fall in the content area, strictly between the two
                // borders; the column may be anywhere in the box width.
                if mouse.row >= box_rect.y.saturating_add(1)
                    && mouse.row < box_rect.y.saturating_add(box_rect.height).saturating_sub(1)
                    && mouse.column >= box_rect.x
                    && mouse.column < box_rect.x.saturating_add(box_rect.width)
                {
                    let content_row = (mouse.row - box_rect.y - 1) as usize;
                    // Subtract the left padding, clamping into [0, inner_width]
                    // so clicks in the padding land at the line start/end.
                    let content_col = mouse
                        .column
                        .saturating_sub(box_rect.x.saturating_add(INPUT_PAD))
                        .min(inner_width as u16) as usize;
                    app.input.cursor = app.input.byte_offset_at_click(
                        inner_width,
                        visible_height,
                        content_row,
                        content_col,
                    );
                    app.ensure_input_cursor_visible();
                    tracing::debug!(
                        cursor = app.input.cursor,
                        row = content_row,
                        col = content_col,
                        "[choreo-tui] mouse click positioned input cursor"
                    );
                }
            }
        }
        Event::Mouse(_) => {}
        _ => {}
    }
    Ok(())
}

/// Handle Ctrl+key combinations on the Chat page.
///
/// Each arm logs a `tracing::debug!` event for observability. Unknown
/// Ctrl+combinations (e.g. Ctrl+Backspace, Ctrl+Home) are forwarded to
/// the input handler so standard text-editing shortcuts still work.
fn handle_chat_ctrl_key(
    key: KeyEvent,
    app: &mut App,
    client_tx: &std::sync::mpsc::Sender<ClientMessage>,
) -> Result<(), ClientError> {
    match key.code {
        KeyCode::Char('r') => {
            let Some(display) = app.active_display_ref() else {
                // No session attached — there is no display whose capability
                // could be consulted.  Mirror the wording the other
                // session-bound shortcuts use (Alt+Enter, Esc, /stop).
                app.status = Some("no session attached".to_string());
                tracing::warn!(
                    session_id = ?app.attached_session_id,
                    "Ctrl+R ignored — no active display (no session attached)",
                );
                return Ok(());
            };
            // Snapshot the display fields before mutating `app` below so the
            // immutable borrow of the display ends before the status writes.
            let capability = display.reasoning_capability.clone();
            let current_effort = display.reasoning_effort.clone();
            let selected_model = display.selected_model.clone();
            match capability.as_ref() {
                // A present-but-empty capability is the daemon's explicit
                // "reasoning not supported" signal.  This must stay distinct
                // from `None`, which only means "capability not yet known".
                Some(c) if c.available_effort_levels.is_empty() => {
                    app.status = Some("model does not support reasoning".to_string());
                }
                Some(c) => {
                    let current = current_effort.unwrap_or_else(|| "off".to_string());
                    if let Some(next) = c.cycle_from(&current) {
                        if let Some(d) = app.active_display() {
                            d.reasoning_effort = Some(next.clone());
                        }
                        app.status = Some(format!("reasoning: {next}"));
                        tracing::info!(
                            session_id = ?app.attached_session_id,
                            current = %current,
                            next = %next,
                            "Ctrl+R cycling reasoning effort",
                        );
                        client_tx
                            .send(ClientMessage::SetReasoningEffort { effort: next })
                            .map_err(broken_pipe)?;
                    } else {
                        // `cycle_from` only returns None for an empty level
                        // set, which the guard above already handled — this
                        // is a defensive fallback.
                        app.status = Some("model does not support reasoning".to_string());
                    }
                }
                // A model is selected but the daemon has not reported its
                // effort levels yet.  `None` here must NOT be conflated with
                // "reasoning unsupported" — the user may simply not have
                // selected a model yet (the original bug this fixes).
                None if selected_model.is_some() => {
                    app.status = Some("reasoning capability not yet available".to_string());
                    tracing::info!(
                        session_id = ?app.attached_session_id,
                        model = ?selected_model,
                        "Ctrl+R pressed before reasoning capability was reported",
                    );
                }
                None => {
                    app.status = Some("no model selected — pick one with Ctrl+M".to_string());
                    tracing::warn!(
                        session_id = ?app.attached_session_id,
                        "Ctrl+R pressed with no model selected",
                    );
                }
            }
        }
        KeyCode::Char('h') => {
            tracing::debug!("Ctrl+H toggling help overlay");
            app.show_ctrl_help = !app.show_ctrl_help;
        }
        KeyCode::Char('s') => {
            tracing::debug!("Ctrl+S navigating to session manager");
            // Highlight the session the user was just viewing so returning
            // to the session list lands on the session they came from (the
            // selection survives the ListSessions round-trip via
            // `pending_select`).
            if let Some(session_id) = app.attached_session_id {
                app.session_mgr.select_session(session_id);
            }
            app.set_page(Page::SessionManager);
            client_tx
                .send(ClientMessage::ListSessions)
                .map_err(broken_pipe)?;
            client_tx
                .send(ClientMessage::SubscribeSessionsSummary)
                .map_err(broken_pipe)?;
        }
        KeyCode::Char('a') => {
            tracing::debug!("Ctrl+A navigating to AI provider accounts");
            app.set_page(Page::AIProviders);
            client_tx
                .send(ClientMessage::ListAccounts)
                .map_err(broken_pipe)?;
        }
        KeyCode::Char('m') => {
            tracing::debug!("Ctrl+M opening model selector");
            // An armed selection is keyed to the Chat page's history, but
            // the selector is a modal overlay that routes mouse events away
            // from the selection arms — a mid-drag Ctrl+M must not leave the
            // gesture dangling underneath it (it would swallow the first
            // click after the selector closes).
            app.text_selection = None;
            app.model_selector.open();
            client_tx
                .send(ClientMessage::ListModels)
                .map_err(broken_pipe)?;
        }
        // Ctrl+C is a deliberate no-op on the chat page (no copy/sigint
        // in raw mode). Absorb it here so it doesn't fall through to the
        // input handler which would insert a literal 'c'.
        KeyCode::Char('c') => {
            tracing::debug!("Ctrl+C ignored on chat page");
        }
        KeyCode::Up => {
            tracing::debug!("Ctrl+Up undo");
            client_tx.send(ClientMessage::Undo).map_err(broken_pipe)?;
        }
        KeyCode::Down => {
            tracing::debug!("Ctrl+Down redo");
            client_tx.send(ClientMessage::Redo).map_err(broken_pipe)?;
        }
        // Ctrl+Left, Ctrl+Right, Ctrl+Backspace, Ctrl+Delete, Ctrl+Home,
        // Ctrl+End, etc. are text-editing shortcuts that should still work
        // in the input box.
        _ => {
            handle_input_key(key, &mut app.input);
            app.ensure_input_cursor_visible();
        }
    }
    Ok(())
}

fn handle_input_key(key: crossterm::event::KeyEvent, input: &mut InputBuffer) {
    // All editing logic moved into InputBuffer::handle_key.
    input.handle_key(key);
}
