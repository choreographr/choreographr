use super::*;

// ── is_command_line: the slash-in-the-first-token rule ────────────

#[test]
fn a_bare_slash_and_a_command_name_are_command_lines() {
    assert!(is_command_line("/"));
    assert!(is_command_line("/model"));
    assert!(is_command_line("/model gpt-4o"));
    // An argument may hold slashes: only the first token is inspected.
    assert!(is_command_line("/add-key svc /etc/key.pem"));
    // A doubled slash is a typing slip, not a path: the leading run of
    // slashes is the marker, so `//acl` stays a command line and reaches the
    // parser, which reports `unknown command: //acl` naming what was typed.
    assert!(is_command_line("//acl"));
    // A slash alone, and a slash followed only by whitespace, are the bare
    // marker (the whole catalog): `command_remainder` yields `""`/`" "`.
    assert!(is_command_line("/ "));
    assert_eq!(command_remainder("/"), Some(""));
    assert_eq!(command_remainder("/model gpt-4o"), Some("model gpt-4o"));
}

// ── the catalog invariant the path rule rests on ──────────────────

#[test]
fn every_catalog_command_is_a_command_line() {
    // The path test above is only safe because no command NAME carries a
    // path separator. A future namespaced name (`/mcp/reconnect`) would
    // silently demote a real command to prompt text — sent to the model
    // instead of run — so hold the invariant over the whole catalog.
    for spec in crate::command_catalog::command_catalog() {
        let line = format!("/{}", spec.name);
        assert!(
            is_command_line(&line),
            "catalog command {line} must classify as a command line"
        );
    }
    // `/add-oauth` is a `/login` alias: it is not in the catalog (the
    // palette lists one name per command) but it must classify too.
    assert!(is_command_line("/add-oauth work"));
}

#[test]
fn a_slash_inside_the_first_token_marks_a_path() {
    // The absolute path a dragged-and-dropped file pastes.
    assert!(!is_command_line("/home/jbrown/veday_tiles.md"));
    // …and the multi-line paste a drop sometimes produces.
    assert!(!is_command_line(
        "/home/jbrown/veday_tiles.md\nexamine this file"
    ));
    // A space-escaped drop (`/home/me/My\\ File.md`): the first token still
    // carries the separator.
    assert!(!is_command_line("/home/me/My\\ File.md"));
    // A `/` typed before a drop concatenates with the pasted path into
    // `//home/...` — still a path, so it stays a prompt.
    assert!(!is_command_line("//home/me/notes.md"));
}

#[test]
fn prose_is_never_a_command_line() {
    assert!(!is_command_line("hello"));
    assert!(!is_command_line(""));
    assert!(!is_command_line(" /model"));
    // A one-component absolute path is a documented blind spot: it has no
    // inner separator, so it reads as a command line and lands on the
    // `unknown command: /tmp` echo, which names the text that was rejected.
    assert!(is_command_line("/tmp"));
}

#[test]
fn a_dropped_path_parses_as_a_prompt_verbatim() {
    // The bug this guards: a dropped file's path is not a command, so it
    // must reach the daemon as prompt text — leading `/` and all — instead
    // of an `unknown command` error.
    let path = "/home/jbrown/veday_tiles.md";
    assert_eq!(
        parse_input_line(path),
        Command::Send(ClientMessageType::RunInput {
            input: path.as_bytes().to_vec(),
        })
    );
}

#[test]
fn a_multi_line_dropped_path_is_a_prompt() {
    let line = "/home/jbrown/veday_tiles.md\nexamine this file";
    assert_eq!(
        parse_input_line(line),
        Command::Send(ClientMessageType::RunInput {
            input: line.as_bytes().to_vec(),
        })
    );
}

#[test]
fn a_command_typo_still_reports_unknown_command() {
    // The path rule is narrow on purpose: a mistyped command name has no
    // separator, so it stays a command line and the user gets the error
    // instead of burning a turn on an accidental prompt.
    assert_eq!(
        parse_input_line("/modl"),
        Command::UnknownCommand("unknown command: /modl".to_string())
    );
}

#[test]
fn a_doubled_slash_is_parsed_verbatim() {
    // The parser is byte-faithful: a doubled leading slash is not normalized
    // away, so the error message names exactly what was typed. The TUI keeps
    // it verbatim too (it only ever normalizes a command NAME's case), so
    // this is also the error a user sees after typing `//acl`.
    assert_eq!(
        parse_input_line("//acl"),
        Command::UnknownCommand("unknown command: //acl".to_string())
    );
}

#[test]
fn refresh_models_parses_plain() {
    assert_eq!(
        parse_input_line("/refresh-models"),
        Command::RefreshModels { force: false },
    );
}

#[test]
fn refresh_models_parses_force() {
    assert_eq!(
        parse_input_line("/refresh-models --force"),
        Command::RefreshModels { force: true },
    );
    assert_eq!(
        parse_input_line("/refresh-models force"),
        Command::RefreshModels { force: true },
    );
}

#[test]
fn refresh_models_rejects_unknown_args() {
    assert!(matches!(
        parse_input_line("/refresh-models --bogus"),
        Command::UnknownCommand(_),
    ));
}

#[test]
fn refresh_models_echo_mentions_force() {
    assert_eq!(
        command_echo(&Command::RefreshModels { force: false }).as_deref(),
        Some("> /refresh-models"),
    );
    assert_eq!(
        command_echo(&Command::RefreshModels { force: true }).as_deref(),
        Some("> /refresh-models --force"),
    );
}

#[test]
fn mcp_parses_bare_as_status() {
    assert_eq!(parse_input_line("/mcp"), Command::Mcp(McpCommand::Status),);
}

#[test]
fn mcp_parses_login_and_logout() {
    assert_eq!(
        parse_input_line("/mcp login docs"),
        Command::Mcp(McpCommand::Login {
            slug: "docs".to_string(),
        }),
    );
    assert_eq!(
        parse_input_line("/mcp logout docs"),
        Command::Mcp(McpCommand::Logout {
            slug: "docs".to_string(),
        }),
    );
    assert!(matches!(
        parse_input_line("/mcp login"),
        Command::UnknownCommand(_)
    ));
    assert!(matches!(
        parse_input_line("/mcp logout a b"),
        Command::UnknownCommand(_)
    ));
}

#[test]
fn mcp_parses_reconnect() {
    assert_eq!(
        parse_input_line("/mcp reconnect docs"),
        Command::Mcp(McpCommand::Reconnect {
            slug: "docs".to_string(),
        }),
    );
}

#[test]
fn mcp_reconnect_requires_exactly_one_slug() {
    assert!(matches!(
        parse_input_line("/mcp reconnect"),
        Command::UnknownCommand(_),
    ));
    assert!(matches!(
        parse_input_line("/mcp reconnect a b"),
        Command::UnknownCommand(_),
    ));
}

#[test]
fn mcp_parses_reload() {
    assert_eq!(
        parse_input_line("/mcp reload"),
        Command::Mcp(McpCommand::Reload),
    );
}

#[test]
fn mcp_reload_takes_no_argument() {
    assert!(matches!(
        parse_input_line("/mcp reload docs"),
        Command::UnknownCommand(_),
    ));
}

#[test]
fn mcp_rejects_unknown_subcommands() {
    // enable/disable have no daemon command, so they surface a usage error
    // rather than being silently ignored.
    for line in ["/mcp enable docs", "/mcp disable docs", "/mcp bogus"] {
        assert!(
            matches!(parse_input_line(line), Command::UnknownCommand(_)),
            "{line} must be a usage error",
        );
    }
}

#[test]
fn mcp_echo_round_trips() {
    assert_eq!(
        command_echo(&Command::Mcp(McpCommand::Status)).as_deref(),
        Some("> /mcp"),
    );
    assert_eq!(
        command_echo(&Command::Mcp(McpCommand::Reconnect {
            slug: "docs".to_string(),
        }))
        .as_deref(),
        Some("> /mcp reconnect docs"),
    );
    assert_eq!(
        command_echo(&Command::Mcp(McpCommand::Reload)).as_deref(),
        Some("> /mcp reload"),
    );
}

#[test]
fn login_and_add_oauth_alias_parse() {
    assert_eq!(
        parse_input_line("/login work"),
        Command::Login {
            account: "work".to_string(),
            device: false,
        }
    );
    assert_eq!(
        parse_input_line("/add-oauth work --device"),
        Command::Login {
            account: "work".to_string(),
            device: true,
        }
    );
}

#[test]
fn login_without_an_account_is_a_usage_error() {
    assert!(matches!(
        parse_input_line("/login"),
        Command::UnknownCommand(msg) if msg.contains("usage")
    ));
}

#[test]
fn login_echo_round_trips() {
    assert_eq!(
        command_echo(&Command::Login {
            account: "work".to_string(),
            device: false,
        })
        .as_deref(),
        Some("> /login work"),
    );
    assert_eq!(
        command_echo(&Command::Login {
            account: "work".to_string(),
            device: true,
        })
        .as_deref(),
        Some("> /login work --device"),
    );
}
