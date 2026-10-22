//! AI Providers page tests, one child module per subject — the boundaries
//! follow the banner sections the monolithic file was already divided into.
//!
//! `list` covers the account list outside the wizard: Enter confirmation, the
//! click/wheel highlight paths, and the list refresh triggered by a credential
//! being added or removed; `paste` covers where a paste lands on the page (the
//! credential modal, the wizard's slug field, the provider filter); `wizard`
//! covers the new-account modal itself — provider picking, filtering, slug
//! validation and submission; `wizard_mouse` covers the wizard's mouse support
//! (wheel scrolling of the highlight, row and filter-row clicks).
//!
//! `wizard` and `wizard_mouse` stay apart even though both drive the same
//! modal: each is independently readable (keyboard/modal flow versus pointer
//! geometry), which is the same split `tests::input` draws between its editing
//! modules and `mouse_click`.

mod list;
mod paste;
mod wizard;
mod wizard_mouse;

use crate::connection::handle_terminal_event;
use crate::state::App;
use choreo_proto::ClientMessage;
use crossterm::event::{Event, KeyModifiers, MouseEvent, MouseEventKind};

/// Drive one mouse event through the full `handle_terminal_event` path
/// (dispatch to the new-account-wizard overlay handler).
///
/// Shared by `list` (clicks and wheel scrolling on the account list) and
/// `wizard_mouse` (the wizard popup's own pointer handling), so both assert
/// against one definition of "the daemon saw this mouse event".
pub(super) fn send_mouse(
    app: &mut App,
    kind: MouseEventKind,
    column: u16,
    row: u16,
    tx: &crossbeam_channel::Sender<ClientMessage>,
) {
    handle_terminal_event(
        Event::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }),
        app,
        tx,
    )
    .expect("handle mouse event");
}
