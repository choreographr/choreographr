use crate::render::{mouse_in_history_box, mouse_in_scrollbar_column};
use crate::state::{App, INPUT_PAD, PAGE_SCROLL_LINES, find_turn_at_row, input_inner_width};
use crate::{clipboard, parse_input_line, selection};
use choreo_client_core::{ClientError, broken_pipe};
use choreo_proto::ClientMessage;
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};

// The command dispatcher lives in a sibling module so this file stays focused
// on terminal-event routing: `run_command` runs a parsed `Command`, and
// `run_named` bridges a resolved keyboard shortcut to it.
use super::command::{run_command, run_named};

pub(super) fn handle_chat_event(
    event: &Event,
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
            // Don't clear help on the help-toggle chord itself — let the
            // toggle arm below handle it.
            let help_toggle =
                key.code == KeyCode::Char('h') && key.modifiers.contains(KeyModifiers::ALT);
            if !help_toggle {
                app.show_ctrl_help = false;
            }
            // A plain (unmodified) keypress predicate, used by the command-line
            // arms below so Ctrl/Alt chords still fall through to the shortcut
            // handler rather than the palette.
            let plain = !key.modifiers.contains(KeyModifiers::CONTROL)
                && !key.modifiers.contains(KeyModifiers::ALT);
            // Command shortcuts are dispatched through the single logical
            // shortcut table (`crate::state::binding_for`) BEFORE the page's
            // own key handling, so a key and its typed-command spelling always
            // run one implementation and one place decides which terminal
            // binds what.  Resolving it here — not only inside the Ctrl handler
            // below — is what routes `Alt+Enter` through the table too, so the
            // non-Ctrl `continue` binding needs no hard-coded match arm.  A
            // bare keypress passes `echo = false` (no `> …` echo).
            if let Some(name) = crate::state::binding_for(key) {
                return run_named(name, false, app, client_tx);
            }
            // A command line is any buffer that starts with `/` — there is no
            // separate mode flag (see `App::command_palette_active`), so the
            // palette appears the instant a `/` starts the line and disappears
            // when the user deletes it.  Read once here, before the match, so
            // the arms below and the normal prompt bindings agree on which
            // regime the keystroke belongs to.
            let command_active = app.command_palette_active();
            match key.code {
                // ── Command palette ──────────────────────────────────
                // While a command line is active these keys belong to the
                // palette: Esc discards the line (never cancels generation),
                // Enter RUNS it, Tab completes the highlighted name into the
                // buffer, and ↑/↓ move the palette highlight.  They are FIRST
                // so they win over the normal prompt bindings, but only fire
                // for plain keys, so Alt+Q and the other chords keep working;
                // every other key falls through to normal editing, so typing
                // (or deleting the leading `/`) edits the command line and can
                // end it.
                KeyCode::Esc if command_active && plain => {
                    app.discard_command_line();
                    return Ok(());
                }
                // Enter (plain OR Shift) RUNS the command line (it does NOT
                // merely complete, and is NOT the normal prompt submit).
                // Shift must not insert a newline here: the command line is a
                // single line, and a stray `\n` would only make the parser
                // reject an otherwise-valid command.  Only Ctrl/Alt chords fall
                // through (to the shortcut dispatch above / the Ctrl handler
                // below).  The line to run is resolved through
                // `command_palette_enter_line`, so Enter runs the HIGHLIGHTED
                // command directly — no preceding `Tab` — while a fully-typed
                // command (`/model gpt-4o`) still runs verbatim with its
                // arguments.  An empty command line with nothing highlighted is
                // a no-op that stays active; a run discards the buffer AFTER
                // the command runs so its echo is preserved.
                KeyCode::Enter if command_active && plain => {
                    let line = app.command_palette_enter_line();
                    if !line.is_empty() {
                        let command = parse_input_line(&line, &mut app.next_request_id);
                        run_command(command, true, app, client_tx)?;
                        app.discard_command_line();
                    }
                    return Ok(());
                }
                KeyCode::Tab if command_active && plain => {
                    if !app.command_palette_matches().is_empty() {
                        app.command_palette_complete();
                    }
                    return Ok(());
                }
                KeyCode::Up if command_active && plain => {
                    app.command_palette_move(-1);
                    return Ok(());
                }
                KeyCode::Down if command_active && plain => {
                    app.command_palette_move(1);
                    return Ok(());
                }
                // A `/` is an ORDINARY character: typing one at the start of an
                // empty prompt begins a command line, and the palette appears
                // because the buffer now starts with `/`.  Mid-prompt (or
                // inside a command line) it is literal too.  Nothing special is
                // needed here — it falls through to the editing arm below.
                //
                // Alt+H toggles the help overlay.  It is not a catalog command
                // (so it is absent from the shortcut table) and is handled here,
                // before the generic `Char` editing arm below.
                KeyCode::Char('h') if key.modifiers.contains(KeyModifiers::ALT) => {
                    tracing::debug!("Alt+H toggling help overlay");
                    app.show_ctrl_help = !app.show_ctrl_help;
                }
                // All Ctrl+ combinations that are NOT command shortcuts are
                // delegated to a dedicated handler.  (Alt+Enter's `continue`
                // binding is handled by the shortcut table at the top of this
                // handler.)
                _ if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    handle_chat_ctrl_key(*key, app);
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
                KeyCode::Up => history_or_line_up(app),
                KeyCode::Down => history_or_line_down(app),
                KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                    // Inserting a newline mutates the buffer: detach a recalled
                    // history entry into the draft first.
                    app.detach_history_on_edit();
                    app.input.insert_char_at_cursor('\n');
                    app.ensure_input_cursor_visible();
                }
                KeyCode::Enter => {
                    let line = app.input.text.trim().to_string();
                    // A plain prompt (any non-empty line not starting with `/`)
                    // begins a new inference turn, so it goes through the shared
                    // client-side submit guard (`App::new_turn_rejection`) — idle
                    // and keystore checks in one place.  Running it *before* the
                    // input buffer is cleared and the per-session draft forgotten
                    // (`clear_current_draft`) is what keeps a rejected prompt in
                    // the input bar to resubmit instead of silently vanishing.
                    // Slash-commands bypass the guard so the user can still e.g.
                    // `/cancel` the in-flight request.
                    //
                    // A `None` status (fresh client, or the brief window before
                    // the daemon reports one) fails open inside the guard: the
                    // daemon stays the authority and will reject a genuinely-busy
                    // submission.
                    //
                    // TODO: replace this blunt guard with prompt queueing, so a
                    // prompt submitted mid-turn is held and dispatched when the
                    // session returns to idle instead of being refused.
                    if !line.is_empty()
                        && !line.starts_with('/')
                        && let Some(reason) = app.new_turn_rejection()
                    {
                        tracing::debug!(reason, "[choreo-tui] prompt rejected client-side");
                        app.status = Some(reason.to_string());
                        app.error = None;
                        return Ok(());
                    }
                    app.input.clear();
                    // The prompt was sent — forget the per-session draft so it
                    // doesn't resurface when the user returns to this session.
                    app.clear_current_draft();
                    app.commit_to_history();
                    let command = parse_input_line(&line, &mut app.next_request_id);
                    run_command(command, true, app, client_tx)?;
                }
                // Every text-editing and cursor key goes through
                // `edit_input`, which detaches a recalled history entry into
                // the draft iff the key actually edited the buffer.  A pure
                // cursor move (Left/Right/Home/End) therefore leaves browsing
                // intact, while typing, Backspace/Delete, or a mutating Ctrl
                // chord ends browsing and keeps the edit as the draft.  There
                // is no hand-kept list of "which keys edit" to drift from
                // `InputBuffer::handle_key`.
                KeyCode::Backspace
                | KeyCode::Delete
                | KeyCode::Left
                | KeyCode::Right
                | KeyCode::Home
                | KeyCode::End
                | KeyCode::Char(_) => {
                    app.edit_input(*key);
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
            if let Some(text) = selection::handle_selection_mouse(app, mouse) {
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
                    // `inner_width` is a terminal column count (always far
                    // below u16::MAX), so the narrowing cast cannot truncate
                    // in practice.
                    #[allow(clippy::cast_possible_truncation)]
                    let inner_w = inner_width as u16;
                    let content_col = mouse
                        .column
                        .saturating_sub(box_rect.x.saturating_add(INPUT_PAD))
                        .min(inner_w) as usize;
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

/// Handle the `Ctrl+<key>` editing chords on the Chat page that are NOT
/// command shortcuts.
///
/// The command-shortcut table (`crate::state::binding_for`) is consulted
/// earlier in `handle_chat_event`, so any `Alt+` command binding has already
/// been dispatched.  What remains is the readline editing layer: the
/// `Ctrl+P`/`Ctrl+N` history walk is driven here (it needs `App` state), the
/// deliberate `Ctrl+C` no-op, and every other chord is forwarded to the input
/// buffer's kernel (`InputBuffer::handle_key`), which owns the movement/kill
/// bindings (`Ctrl+A/E/B/F/D/H/K/T/W/U`).
fn handle_chat_ctrl_key(key: KeyEvent, app: &mut App) {
    match key.code {
        // Ctrl+P / Ctrl+N are readline `previous-history` / `next-history` —
        // the same behaviour as ↑/↓ (see `history_or_line_up`/`_down`).
        KeyCode::Char('p') => history_or_line_up(app),
        KeyCode::Char('n') => history_or_line_down(app),
        // Ctrl+C is a deliberate no-op on the chat page (no copy/sigint in raw
        // mode).  Absorb it here so it does not fall through to the input
        // handler, which would insert a literal 'c'.
        KeyCode::Char('c') => {
            tracing::debug!("Ctrl+C ignored on chat page");
        }
        // Ctrl+Backspace clears the whole draft — but while browsing history
        // it is INERT: it must not clear the recalled entry or detach it.  The
        // user leaves browsing with Down (or by editing the entry), never by
        // wiping it.
        KeyCode::Backspace
            if key.modifiers.contains(KeyModifiers::CONTROL) && app.history_index.is_some() =>
        {
            tracing::debug!("[choreo-tui] Ctrl+Backspace ignored while browsing history");
        }
        // Every other editing chord — Ctrl+Left/Right/Backspace/Delete/Home/
        // End/W/U/A/E/B/F/D/H/K/T — goes through `edit_input`, which detaches a
        // recalled history entry into the draft iff the chord actually edited
        // the buffer.  The pure cursor moves leave browsing intact; the
        // kill/delete chords detach it.
        _ => app.edit_input(key),
    }
}

/// `Up` / readline `Ctrl+P` (`previous-history`): on the first visual line,
/// recall the previous prompt when the draft is empty (or a recall is already
/// in progress), else jump to the start of the line; on a lower visual line,
/// move the cursor up one wrapped line.
fn history_or_line_up(app: &mut App) {
    let inner = app
        .last_terminal_size
        .map_or(78, |(w, _)| input_inner_width(w));
    if app.input.is_on_first_visual_line(inner) {
        // Recall requires an empty draft: history is reachable only from an
        // empty prompt, or while already browsing.  With a non-empty draft, Up
        // on the first visual line moves the cursor to the start of the line
        // instead — the mirror of Down-on-last-line's move-to-end.
        if app.history_index.is_some() || app.input.text.is_empty() {
            app.navigate_history_up();
        } else {
            app.input.cursor_home_line();
            app.ensure_input_cursor_visible();
        }
    } else {
        app.input.cursor_up(inner);
        app.ensure_input_cursor_visible();
    }
}

/// `Down` / readline `Ctrl+N` (`next-history`): the mirror of
/// [`history_or_line_up`].  Down only drives history navigation while an entry
/// is loaded; on the last visual line while editing the draft it lands at
/// end-of-line instead of being a dead key.
fn history_or_line_down(app: &mut App) {
    let inner = app
        .last_terminal_size
        .map_or(78, |(w, _)| input_inner_width(w));
    if app.input.is_on_last_visual_line(inner) {
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
