//! Metadata catalog for the unified command model.
//!
//! Every command the client understands — whether it ships a [`ClientMessage`]
//! to the daemon or drives local UI — is described here exactly once. The
//! catalog is the single source of truth for command discovery and
//! descriptions (e.g. the TUI command palette); the parser in [`crate::shell`]
//! is the single source of truth for *behavior*. The drift-guard tests in
//! `tests.rs` pin the two together: every catalog command must parse, and the
//! catalog must equal the explicit list of parser command names (which a new
//! parse arm is required to extend). A catalog entry with no parse arm fails
//! the suite; a parse arm is caught once it is named in that list.
//!
//! [`ClientMessage`]: choreo_proto::ClientMessage

use std::sync::LazyLock;

/// Coarse grouping used to organize commands in discovery surfaces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandGroup {
    Session,
    Account,
    Security,
    System,
}

impl CommandGroup {
    /// Human-readable group label (the discovery surface's section header).
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            CommandGroup::Session => "Session",
            CommandGroup::Account => "Account",
            CommandGroup::Security => "Security",
            CommandGroup::System => "System",
        }
    }
}

/// Static metadata for one command.
///
/// `name` is the bare invocation (`/<name>`); `arg_hint` documents the accepted
/// arguments/subcommands (or `None` for a bare command); `summary` is the
/// one-line description shown to users.
pub struct CommandSpec {
    pub name: &'static str,
    pub summary: &'static str,
    pub arg_hint: Option<&'static str>,
    pub group: CommandGroup,
}

/// The catalog, in the fixed order discovery surfaces present it: alphabetical
/// by command name.  The inline command palette is the discovery surface, and
/// an A→Z list lets a user scan it predictably; each entry still carries its
/// [`CommandGroup`] as metadata, but that grouping does not drive the ordering.
static COMMAND_CATALOG: LazyLock<Vec<CommandSpec>> = LazyLock::new(|| {
    vec![
        CommandSpec {
            name: "account",
            summary: "Manage accounts: open the accounts page, or list/remove/set",
            arg_hint: Some("[list|remove <name>|<name>]"),
            group: CommandGroup::Account,
        },
        CommandSpec {
            name: "acl",
            summary: "Manage the client ACL",
            arg_hint: Some("add <base64-pubkey>"),
            group: CommandGroup::Security,
        },
        CommandSpec {
            name: "add-key",
            summary: "Store an API-key credential",
            arg_hint: Some("<service> <api_key>"),
            group: CommandGroup::Account,
        },
        CommandSpec {
            name: "add-x",
            summary: "Store an X/Twitter credential",
            arg_hint: Some("<service> <key> <secret> <token> <token_secret> <bearer>"),
            group: CommandGroup::Account,
        },
        CommandSpec {
            name: "cancel",
            summary: "Cancel a request by id",
            arg_hint: Some("<request-id>"),
            group: CommandGroup::Session,
        },
        CommandSpec {
            name: "continue",
            summary: "Continue the current session",
            arg_hint: None,
            group: CommandGroup::Session,
        },
        CommandSpec {
            name: "lock",
            summary: "Lock the daemon keystore",
            arg_hint: None,
            group: CommandGroup::Security,
        },
        CommandSpec {
            name: "model",
            summary: "Choose the session's model (opens the picker when omitted)",
            arg_hint: Some("[model]"),
            group: CommandGroup::Session,
        },
        CommandSpec {
            name: "ping",
            summary: "Ping the daemon",
            arg_hint: None,
            group: CommandGroup::Session,
        },
        CommandSpec {
            name: "quit",
            summary: "Exit the TUI",
            arg_hint: None,
            group: CommandGroup::System,
        },
        CommandSpec {
            name: "reasoning",
            summary: "Cycle, list, or set the reasoning effort",
            arg_hint: Some("[list|<level>]"),
            group: CommandGroup::Session,
        },
        CommandSpec {
            name: "redo",
            summary: "Redo the last undone turn",
            arg_hint: None,
            group: CommandGroup::Session,
        },
        CommandSpec {
            name: "refresh-models",
            summary: "Refresh the models.dev catalog",
            arg_hint: Some("[--force]"),
            group: CommandGroup::System,
        },
        CommandSpec {
            name: "remove-key",
            summary: "Remove a stored credential",
            arg_hint: Some("<service>"),
            group: CommandGroup::Account,
        },
        CommandSpec {
            name: "session",
            summary: "Manage the session: open the manager, or list/new/switch/info",
            arg_hint: Some("[list|new [title]|switch <id>|info <id>]"),
            group: CommandGroup::Session,
        },
        CommandSpec {
            name: "stop",
            summary: "Stop the running request",
            arg_hint: None,
            group: CommandGroup::Session,
        },
        CommandSpec {
            name: "undo",
            summary: "Undo the last turn",
            arg_hint: None,
            group: CommandGroup::Session,
        },
        CommandSpec {
            name: "unlock",
            summary: "Unlock the daemon keystore",
            arg_hint: Some("[base64-key]"),
            group: CommandGroup::Security,
        },
    ]
});

/// The static command catalog, in catalog (presentation) order.
#[must_use]
pub fn command_catalog() -> &'static [CommandSpec] {
    COMMAND_CATALOG.as_slice()
}

/// A single match produced by [`match_commands`].
pub struct CommandMatch {
    pub spec: &'static CommandSpec,
    /// Char indices into `spec.name` to emphasize; empty when the query is empty.
    pub name_positions: Vec<usize>,
}

/// Case-insensitive EXACT-then-PREFIX match against command names only.
///
/// - empty query => every spec, catalog order, empty positions
/// - exact name match ranks first, then prefix matches, each in catalog order
/// - positions = `0..query.chars().count()` for both exact and prefix matches
#[must_use]
pub fn match_commands(query: &str) -> Vec<CommandMatch> {
    // Command names are ASCII, so lowercasing is a stable, lossless fold here.
    let needle = query.to_lowercase();
    // Emphasized prefix length in *chars* (the caller highlights the matched
    // leading characters of the name, not a byte range).
    let prefix_len = query.chars().count();

    // Empty query short-circuits: every command, no highlight, catalog order.
    if needle.is_empty() {
        return command_catalog()
            .iter()
            .map(|spec| CommandMatch {
                spec,
                name_positions: Vec::new(),
            })
            .collect();
    }

    // Two passes so an exact match always precedes a mere prefix match while
    // each tier preserves catalog order.
    let mut exact = Vec::new();
    let mut prefix = Vec::new();
    for spec in command_catalog() {
        let name = spec.name.to_lowercase();
        if name == needle {
            exact.push(CommandMatch {
                spec,
                name_positions: (0..prefix_len).collect(),
            });
        } else if name.starts_with(&needle) {
            prefix.push(CommandMatch {
                spec,
                name_positions: (0..prefix_len).collect(),
            });
        }
    }
    exact.extend(prefix);
    exact
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_is_non_empty() {
        assert!(!command_catalog().is_empty());
    }

    #[test]
    fn catalog_is_alphabetical() {
        // The palette presents the catalog verbatim, so the catalog order IS
        // the on-screen order: it must stay sorted A→Z or the picker reads out
        // of order.  Guard it here, next to the data it constrains.
        let names: Vec<&str> = command_catalog().iter().map(|s| s.name).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(
            names, sorted,
            "catalog presentation order must be alphabetical"
        );
    }

    #[test]
    fn names_are_unique() {
        let mut names: Vec<&str> = command_catalog().iter().map(|s| s.name).collect();
        let count = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), count, "catalog command names must be unique");
    }

    #[test]
    fn group_labels() {
        assert_eq!(CommandGroup::Session.label(), "Session");
        assert_eq!(CommandGroup::Account.label(), "Account");
        assert_eq!(CommandGroup::Security.label(), "Security");
        assert_eq!(CommandGroup::System.label(), "System");
    }

    #[test]
    fn empty_query_returns_all_in_catalog_order() {
        let matches = match_commands("");
        let got: Vec<&str> = matches.iter().map(|m| m.spec.name).collect();
        let expected: Vec<&str> = command_catalog().iter().map(|s| s.name).collect();
        assert_eq!(got, expected);
        assert!(
            matches.iter().all(|m| m.name_positions.is_empty()),
            "empty query yields no highlight positions"
        );
    }

    #[test]
    fn prefix_match_emphasizes_prefix() {
        let matches = match_commands("mo");
        assert_eq!(matches.first().map(|m| m.spec.name), Some("model"));
        assert_eq!(matches[0].name_positions, vec![0, 1]);
        // "mo" must not pull in unrelated commands.
        assert!(matches.iter().all(|m| m.spec.name.starts_with("mo")));
    }

    #[test]
    fn exact_match_ranks_first() {
        let matches = match_commands("model");
        assert_eq!(matches.first().map(|m| m.spec.name), Some("model"));
        assert_eq!(matches[0].name_positions, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn match_is_case_insensitive() {
        assert_eq!(
            match_commands("MODEL").first().map(|m| m.spec.name),
            Some("model"),
        );
    }

    #[test]
    fn unknown_query_is_empty() {
        assert!(match_commands("zzz").is_empty());
    }
}
