//! The client-side command-line parser shared by the front-ends.
//!
//! [`parse_input_line`] turns a line typed into a TUI input box into a
//! [`Command`]: a slash-prefixed command becomes a structured variant
//! (either a [`ClientMessageType`] to send or a local-UI action), while a plain
//! line becomes a `RunInput` message carrying a fresh request id. It is pure
//! syntax — decoding and validation of keys and credentials happen where
//! those values are owned — so the same parser drives every front-end, and
//! [`command_echo`] renders the canonical transcript form of a parsed command.
//!
//! [`command_remainder`] is the one classification primitive: it decides whether
//! a line is a command at all and hands back what follows the marker, so the parser
//! below and every front-end's command-ness decision ([`is_command_line`]) agree by
//! construction.

mod command;
mod parse;

pub use command::{Command, McpCommand, UnlockMethod, command_echo};
pub use parse::is_valid_account_name;

use choreo_proto::ClientMessageType;
use tracing::debug;

use parse::parse_command;

/// The command remainder of `line` — the text after its leading `/` — or `None`
/// when `line` is a prompt rather than a command line.
///
/// A line is a command line when it starts with `/` **and** its first token is
/// not a filesystem path.  The second test exists because a path is the one
/// thing a chat composer routinely receives that begins with a slash: a file
/// dragged onto the terminal arrives as a paste of its own absolute path
/// (`/home/me/notes.md`), and reading that as a command would make the drop
/// useless — the user wanted the path in a prompt.
///
/// It cannot misfire on a real command: no command name contains a path
/// separator (the `every_catalog_command_is_a_command_line` test holds that
/// over the whole catalog), so a separator *inside* the first token — after its
/// leading `/`s, which are the command marker plus a doubled-slash slip — is a
/// positive path signature, never a command.  Only that first token is
/// inspected: an argument may hold slashes freely (`/add-key svc /etc/key.pem`
/// is still a command).
///
/// A one-component absolute path (`/tmp`) has no inner separator and so still
/// reads as a command line; a bare top-level name is not what a drag-and-drop
/// delivers (it drops a file, not `/`'s own children), and the resulting
/// `unknown command: /tmp` echo names the offending text, so the ambiguity
/// costs a clear error rather than a silent drop.
///
/// Exactly ONE leading `/` is the command marker; a run of them is that marker
/// plus a typing slip, which is why the path test ignores the run rather than
/// reading it as a separator.  The remainder keeps the whole run, so a line
/// reaching [`parse_input_line`] is parsed byte-faithfully (`//acl` reports
/// `unknown command: //acl`, naming what the user actually typed).
///
/// This is the single primitive behind command-ness: [`is_command_line`] is
/// `is_some()` on it, the parser below takes its remainder from it, and the
/// TUI's palette derives its query from it — so what the composer shows as a
/// command and what the parser runs can never disagree.
#[must_use]
pub fn command_remainder(line: &str) -> Option<&str> {
    let rest = line.strip_prefix('/')?;
    let first = rest.split(char::is_whitespace).next().unwrap_or("");
    (!first.trim_start_matches('/').contains('/')).then_some(rest)
}

/// Whether `line` is a slash-command invocation rather than a prompt — whether
/// [`command_remainder`] finds a command in it.  See that function for the
/// path-versus-command rule; every command-ness decision a front-end makes (the
/// parser, the TUI's palette and `command` input title, its client-side submit
/// guard) is derived from this one predicate.
#[must_use]
pub fn is_command_line(line: &str) -> bool {
    command_remainder(line).is_some()
}

/// Parse a line of user input into a [`Command`].
///
/// A non-empty line that is not a command line (see [`is_command_line`]: a
/// slash-led line whose first token carries a path separator is a prompt, not a
/// command — that is a dropped file's path) becomes a
/// `ClientMessageType::RunInput` carrying the line **verbatim**, a leading `/`
/// included.  The run's `stream_id` is NOT chosen here: the daemon assigns it
/// when it accepts the run and reports it in the acceptance reply
/// (`SessionEvent::Started`), so the parser needs no counter. Slash commands
/// (`/name …`) are parsed by the `parse` submodule's dispatcher; unknown ones
/// come back as [`Command::UnknownCommand`].
#[must_use]
pub fn parse_input_line(line: &str) -> Command {
    let line = line.trim();
    if line.is_empty() {
        return Command::Empty;
    }

    if let Some(rest) = command_remainder(line) {
        // The classification and the remainder come from the same call, so a
        // line can never be parsed as a command it does not look like.
        let cmd = parse_command(rest);
        debug!("parsed command: {cmd:?}");
        return cmd;
    }

    Command::Send(ClientMessageType::RunInput {
        input: line.as_bytes().to_vec(),
    })
}

#[cfg(test)]
mod tests;
