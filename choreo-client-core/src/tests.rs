use super::*;
use choreo_proto::ClientMessage;

#[test]
fn parses_empty_line() {
    let mut next = 1;
    assert_eq!(parse_input_line("   ", &mut next), Command::Empty);
    assert_eq!(next, 1);
}

#[test]
fn parses_ping() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/ping", &mut next),
        Command::Send(ClientMessage::Ping)
    );
    assert_eq!(next, 3);
}

#[test]
fn parses_cancel() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/cancel 42", &mut next),
        Command::Send(ClientMessage::Cancel { request_id: 42 })
    );
    assert_eq!(next, 3);
}

#[test]
fn rejects_invalid_cancel() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/cancel nope", &mut next),
        Command::InvalidCancel("nope".to_string())
    );
    assert_eq!(next, 3);
}

#[test]
fn parses_unlock_raw() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/unlock", &mut next),
        Command::Unlock {
            method: UnlockMethod::Raw,
        }
    );
    assert_eq!(next, 3);
}

#[test]
fn parses_unlock_with_base64_key() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/unlock aGVsbG8=", &mut next),
        Command::Unlock {
            method: UnlockMethod::Key("aGVsbG8=".to_string()),
        }
    );
    assert_eq!(next, 3);
}

#[test]
fn models_command_is_removed() {
    // `/models` was an alias for `/model`; the alias was dropped, so it is no
    // longer recognised (and must NOT be silently accepted).
    let mut next = 10;
    assert_eq!(
        parse_input_line("/models", &mut next),
        Command::UnknownCommand("unknown command: /models".to_string())
    );
    assert_eq!(next, 10);
}

#[test]
fn models_command_with_arg_is_removed() {
    let mut next = 10;
    assert_eq!(
        parse_input_line("/models gpt-5.4-nano", &mut next),
        Command::UnknownCommand("unknown command: /models gpt-5.4-nano".to_string())
    );
    assert_eq!(next, 10);
}

#[test]
fn model_bare_opens_selector() {
    let mut next = 10;
    assert_eq!(
        parse_input_line("/model", &mut next),
        Command::OpenModelSelector
    );
    assert_eq!(next, 10);
}

#[test]
fn model_set() {
    let mut next = 10;
    assert_eq!(
        parse_input_line("/model gpt-5.4-nano", &mut next),
        Command::Send(ClientMessage::SetModel {
            model: "gpt-5.4-nano".to_string(),
        })
    );
    assert_eq!(next, 10);
}

#[test]
fn rejects_unknown_command() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/bogus", &mut next),
        Command::UnknownCommand("unknown command: /bogus".to_string())
    );
    assert_eq!(next, 3);
}

#[test]
fn session_bare_opens_manager() {
    // Bare `/session` opens the session manager.
    let mut next = 3;
    assert_eq!(
        parse_input_line("/session", &mut next),
        Command::OpenSessions
    );
    assert_eq!(next, 3);
}

#[test]
fn session_info_parses_id() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/session info 7", &mut next),
        Command::Send(ClientMessage::GetSessionState { session_id: 7 })
    );
    assert_eq!(next, 3);
}

#[test]
fn session_info_rejects_invalid_id() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/session info nope", &mut next),
        Command::UnknownCommand("usage: /session info <id>".to_string())
    );
    assert_eq!(next, 3);
}

#[test]
fn session_list() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/session list", &mut next),
        Command::Send(ClientMessage::ListSessions)
    );
    assert_eq!(next, 3);
}

#[test]
fn session_new_with_title() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/session new my title", &mut next),
        Command::Send(ClientMessage::CreateSession {
            title: Some("my title".to_string()),
            parent_session_id: None,
            working_dir: None,
            context_config: None,
            account_name: None,
            selected_model: None,
            reasoning_effort: None,
        })
    );
    assert_eq!(next, 3);
}

#[test]
fn session_new_without_title() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/session new", &mut next),
        Command::Send(ClientMessage::CreateSession {
            title: None,
            parent_session_id: None,
            working_dir: None,
            context_config: None,
            account_name: None,
            selected_model: None,
            reasoning_effort: None,
        })
    );
    assert_eq!(next, 3);
}

#[test]
fn session_switch() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/session switch 5", &mut next),
        Command::Send(ClientMessage::AttachSession { session_id: 5 })
    );
    assert_eq!(next, 3);
}

#[test]
fn new_without_title() {
    // Bare `/new` is the top-level shortcut for `/session new`.
    let mut next = 3;
    assert_eq!(
        parse_input_line("/new", &mut next),
        Command::Send(ClientMessage::CreateSession {
            title: None,
            parent_session_id: None,
            working_dir: None,
            context_config: None,
            account_name: None,
            selected_model: None,
            reasoning_effort: None,
        })
    );
    assert_eq!(next, 3);
}

#[test]
fn new_with_title() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/new my title", &mut next),
        Command::Send(ClientMessage::CreateSession {
            title: Some("my title".to_string()),
            parent_session_id: None,
            working_dir: None,
            context_config: None,
            account_name: None,
            selected_model: None,
            reasoning_effort: None,
        })
    );
    assert_eq!(next, 3);
}

#[test]
fn new_and_session_new_agree() {
    // `/new [title]` and `/session new [title]` must produce the same command
    // so the two entry points can never drift apart.
    for (bare, grouped) in [("/new", "/session new"), ("/new t", "/session new t")] {
        let mut nb = 3;
        let mut ng = 3;
        assert_eq!(
            parse_input_line(bare, &mut nb),
            parse_input_line(grouped, &mut ng),
            "`{bare}` and `{grouped}` must parse identically"
        );
    }
}

#[test]
fn session_switch_rejects_invalid_id() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/session switch nope", &mut next),
        Command::UnknownCommand("usage: /session switch <id>".to_string())
    );
    assert_eq!(next, 3);
}

#[test]
fn session_unknown_subcommand() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/session bogus", &mut next),
        Command::UnknownCommand(
            "session subcommands: list, new [title], switch <id>, info <id>".to_string()
        )
    );
    assert_eq!(next, 3);
}

#[test]
fn parses_add_key() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/add-key openai sk-abc123", &mut next),
        Command::AddCredential {
            service: "openai".to_string(),
            credential_type: "api_key".to_string(),
            fields: vec!["sk-abc123".to_string()],
        }
    );
    assert_eq!(next, 3);
}

#[test]
fn ignores_trailing_unlock_arg_for_add_key() {
    // The `[unlock]` argument was removed with the per-daemon unlock-key
    // design: key resolution is per-addr inside build_add_credential_message.
    let mut next = 3;
    assert_eq!(
        parse_input_line("/add-key openai sk-abc123 unlock", &mut next),
        Command::AddCredential {
            service: "openai".to_string(),
            credential_type: "api_key".to_string(),
            fields: vec!["sk-abc123".to_string()],
        }
    );
    assert_eq!(next, 3);
}

#[test]
fn rejects_add_key_without_enough_args() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/add-key openai", &mut next),
        Command::UnknownCommand("usage: /add-key <service> <api_key>".to_string())
    );
    assert_eq!(next, 3);
}

#[test]
fn parses_add_x() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/add-x twitter ck cs at ats -", &mut next),
        Command::AddCredential {
            service: "twitter".to_string(),
            credential_type: "x".to_string(),
            fields: vec![
                "ck".to_string(),
                "cs".to_string(),
                "at".to_string(),
                "ats".to_string(),
                "-".to_string(),
            ],
        }
    );
    assert_eq!(next, 3);
}

#[test]
fn parses_add_x_with_bearer() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/add-x twitter ck cs at ats mybearer", &mut next),
        Command::AddCredential {
            service: "twitter".to_string(),
            credential_type: "x".to_string(),
            fields: vec![
                "ck".to_string(),
                "cs".to_string(),
                "at".to_string(),
                "ats".to_string(),
                "mybearer".to_string(),
            ],
        }
    );
    assert_eq!(next, 3);
}

#[test]
fn rejects_add_x_without_enough_args() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/add-x twitter ck cs", &mut next),
        Command::UnknownCommand("usage: /add-x <service> <api_key> <api_key_secret> <access_token> <access_token_secret> <bearer_or_->_".to_string())
    );
    assert_eq!(next, 3);
}

#[test]
fn parses_remove_key() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/remove-key openai", &mut next),
        Command::RemoveCredential {
            service: "openai".to_string(),
        }
    );
    assert_eq!(next, 3);
}

#[test]
fn parses_acl_add_with_valid_key() {
    let mut next = 3;
    // b64 of exactly 32 bytes.
    let key_b64 = "AQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyA=";
    assert_eq!(
        parse_input_line(&format!("/acl add {key_b64}"), &mut next),
        Command::AclAdd {
            pubkey: key_b64.to_string(),
        }
    );
    assert_eq!(next, 3);
}

#[test]
fn rejects_acl_add_with_bad_base64_or_wrong_length() {
    let mut next = 3;
    assert!(matches!(
        parse_input_line("/acl add not-base64!!!", &mut next),
        Command::UnknownCommand(_)
    ));
    // Valid base64 but 16 bytes.
    assert!(matches!(
        parse_input_line("/acl add c29tZTE2Ynl0ZXNr", &mut next),
        Command::UnknownCommand(_)
    ));
    // Missing argument.
    assert!(matches!(
        parse_input_line("/acl add", &mut next),
        Command::UnknownCommand(_)
    ));
}

#[test]
fn rejects_remove_key_with_invalid_service_name() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/remove-key my service", &mut next),
        Command::UnknownCommand(
            "account name must be lowercase alphanumeric, hyphens, or underscores".to_string()
        )
    );
}

#[test]
fn rejects_add_key_with_invalid_service_name() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/add-key Jonathan sk-test", &mut next),
        Command::UnknownCommand(
            "account name must be lowercase alphanumeric, hyphens, or underscores".to_string()
        )
    );
}

#[test]
fn rejects_account_set_with_invalid_name() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/account Jonathan's Opencode", &mut next),
        Command::UnknownCommand(
            "account name must be lowercase alphanumeric, hyphens, or underscores".to_string()
        )
    );
}

#[test]
fn rejects_remove_key_without_service() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/remove-key", &mut next),
        Command::UnknownCommand("usage: /remove-key <service>".to_string())
    );
}

#[test]
fn is_valid_account_name_valid() {
    assert!(is_valid_account_name("my-account"));
    assert!(is_valid_account_name("a"));
    assert!(is_valid_account_name("0"));
    assert!(is_valid_account_name("jonathan-opencode-zen"));
    assert!(is_valid_account_name("chau_opencode_go"));
    assert!(is_valid_account_name("a1-b2_c3"));
}

#[test]
fn is_valid_account_name_rejects_invalid() {
    assert!(!is_valid_account_name(""));
    assert!(!is_valid_account_name("Jonathan"));
    assert!(!is_valid_account_name("my account"));
    assert!(!is_valid_account_name("Chau's Opencode go"));
    assert!(!is_valid_account_name("has space"));
    assert!(!is_valid_account_name("has.period"));
    assert!(!is_valid_account_name("UPPERCASE"));
}

#[test]
fn parses_run_input_and_increments_request_id() {
    let mut next = 10;
    assert_eq!(
        parse_input_line("hello world", &mut next),
        Command::Send(ClientMessage::RunInput {
            request_id: 10,
            input: b"hello world".to_vec(),
        })
    );
    assert_eq!(next, 11);
}

// ── Account sub-commands ──────────────────────────────────────────────────

#[test]
fn account_list() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/account list", &mut next),
        Command::Send(ClientMessage::ListAccounts)
    );
    assert_eq!(next, 3);
}

#[test]
fn account_bare_opens_page() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/account", &mut next),
        Command::OpenAccounts
    );
    assert_eq!(next, 3);
}

#[test]
fn account_remove() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/account remove my-provider", &mut next),
        Command::Send(ClientMessage::RemoveAccount {
            name: "my-provider".to_string()
        })
    );
    assert_eq!(next, 3);
}

#[test]
fn account_remove_missing_name() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/account remove", &mut next),
        Command::UnknownCommand("usage: /account remove <name>".to_string())
    );
    assert_eq!(next, 3);
}

#[test]
fn account_set_valid_name() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/account my-account", &mut next),
        Command::Send(ClientMessage::SetSessionAccount {
            name: "my-account".to_string()
        })
    );
    assert_eq!(next, 3);
}

// ── Reasoning effort ──────────────────────────────────────────────────────

#[test]
fn reasoning_bare_cycles() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/reasoning", &mut next),
        Command::ReasoningCycle
    );
    assert_eq!(next, 3);
}

#[test]
fn reasoning_list() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/reasoning list", &mut next),
        Command::ReasoningList
    );
    assert_eq!(next, 3);
}

#[test]
fn reasoning_set_off() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/reasoning off", &mut next),
        Command::Send(ClientMessage::SetReasoningEffort {
            effort: "off".to_string()
        })
    );
    assert_eq!(next, 3);
}

#[test]
fn reasoning_set_low() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/reasoning low", &mut next),
        Command::Send(ClientMessage::SetReasoningEffort {
            effort: "low".to_string()
        })
    );
    assert_eq!(next, 3);
}

#[test]
fn reasoning_set_medium() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/reasoning medium", &mut next),
        Command::Send(ClientMessage::SetReasoningEffort {
            effort: "medium".to_string()
        })
    );
    assert_eq!(next, 3);
}

#[test]
fn reasoning_set_high() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/reasoning high", &mut next),
        Command::Send(ClientMessage::SetReasoningEffort {
            effort: "high".to_string()
        })
    );
    assert_eq!(next, 3);
}

#[test]
fn reasoning_set_none_alias() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/reasoning none", &mut next),
        Command::Send(ClientMessage::SetReasoningEffort {
            effort: "off".to_string()
        })
    );
    assert_eq!(next, 3);
}

#[test]
fn reasoning_set_disabled_alias() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/reasoning disabled", &mut next),
        Command::Send(ClientMessage::SetReasoningEffort {
            effort: "off".to_string()
        })
    );
    assert_eq!(next, 3);
}

#[test]
fn reasoning_set_med_alias() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/reasoning med", &mut next),
        Command::Send(ClientMessage::SetReasoningEffort {
            effort: "medium".to_string()
        })
    );
    assert_eq!(next, 3);
}

#[test]
fn reasoning_set_on() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/reasoning on", &mut next),
        Command::Send(ClientMessage::SetReasoningEffort {
            effort: "on".to_string()
        })
    );
    assert_eq!(next, 3);
}

#[test]
fn reasoning_unknown_slug_passes_through() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/reasoning turbo", &mut next),
        Command::Send(ClientMessage::SetReasoningEffort {
            effort: "turbo".to_string()
        })
    );
    assert_eq!(next, 3);
}

#[test]
fn reasoning_max_slug_passes_through() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/reasoning max", &mut next),
        Command::Send(ClientMessage::SetReasoningEffort {
            effort: "max".to_string()
        })
    );
    assert_eq!(next, 3);
}

// ── Undo / Redo commands ────────────────────────────────────────────────

#[test]
fn parses_undo() {
    let mut next = 5;
    assert_eq!(parse_input_line("/undo", &mut next), Command::Undo);
    assert_eq!(next, 5);
}

#[test]
fn parses_redo() {
    let mut next = 5;
    assert_eq!(parse_input_line("/redo", &mut next), Command::Redo);
    assert_eq!(next, 5);
}

#[test]
fn command_echo_undo() {
    assert_eq!(command_echo(&Command::Undo), Some("> undo".to_string()));
}

#[test]
fn command_echo_redo() {
    assert_eq!(command_echo(&Command::Redo), Some("> redo".to_string()));
}

// ── Continue / Stop commands ────────────────────────────────────────────

#[test]
fn parses_continue() {
    let mut next = 3;
    assert_eq!(parse_input_line("/continue", &mut next), Command::Continue);
    assert_eq!(next, 3);
}

#[test]
fn parses_stop() {
    let mut next = 3;
    assert_eq!(parse_input_line("/stop", &mut next), Command::Stop);
    assert_eq!(next, 3);
}

#[test]
fn shell_command_continue_echo() {
    assert_eq!(
        command_echo(&Command::Continue),
        Some("> continue".to_string())
    );
}

#[test]
fn shell_command_stop_echo() {
    assert_eq!(command_echo(&Command::Stop), Some("> stop".to_string()));
}

#[test]
fn parses_quit() {
    let mut next = 3;
    assert_eq!(parse_input_line("/quit", &mut next), Command::Quit);
    assert_eq!(next, 3);
}

// ── Command catalog drift guard ──────────────────────────────────────────
//
// The catalog (`choreo_client_core::command_catalog`) and the parser are two
// halves of the unified command model; these tests keep them in contact.  The
// guarantee is symmetric but *list-bounded* (see `PARSER_COMMAND_NAMES`).

/// The explicit list of parser command names — the parser's half of the drift
/// guard, which a new parse arm must extend.  The guarantee is only as strong
/// as this list: a catalog command with no parse arm is caught by test (a)
/// below, and the listed parser names are compared to the catalog by test (b),
/// but a parse arm added to the parser without also being named here would not
/// be detected.
const PARSER_COMMAND_NAMES: &[&str] = &[
    "session",
    "new",
    "model",
    "reasoning",
    "continue",
    "stop",
    "cancel",
    "undo",
    "redo",
    "ping",
    "account",
    "add-key",
    "add-x",
    "remove-key",
    "unlock",
    "lock",
    "acl",
    "refresh-models",
    "quit",
];

/// A syntactically valid invocation for a command name, so the guard can prove
/// the parser *recognises* it. Commands that require arguments get a dummy one,
/// and `/acl` needs its `add` subcommand, so the probe is a valid line rather
/// than merely the bare name.
fn probe_invocation(name: &str) -> String {
    match name {
        "cancel" => "/cancel 42".to_string(),
        "add-key" => "/add-key svc key".to_string(),
        "add-x" => "/add-x svc a b c d e".to_string(),
        "remove-key" => "/remove-key svc".to_string(),
        // b64 of exactly 32 bytes (a syntactically valid ACL pubkey).
        "acl" => "/acl add AQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyA=".to_string(),
        other => format!("/{other}"),
    }
}

/// (a) Every catalog command name is recognised by the parser.
#[test]
fn every_catalog_command_is_parseable() {
    for spec in command_catalog() {
        let line = probe_invocation(spec.name);
        let mut next = 0;
        let cmd = parse_input_line(&line, &mut next);
        assert!(
            !matches!(cmd, Command::UnknownCommand(_)),
            "parser rejects catalog command `{}` (probe `{line}` -> {cmd:?})",
            spec.name,
        );
    }
}

/// (b) The listed parser command names and the catalog's names are equal.
#[test]
fn parser_commands_match_catalog_names() {
    let mut catalog: Vec<&str> = command_catalog().iter().map(|s| s.name).collect();
    catalog.sort_unstable();
    let mut parser: Vec<&str> = PARSER_COMMAND_NAMES.to_vec();
    parser.sort_unstable();
    assert_eq!(
        parser, catalog,
        "the parser's accepted commands and the catalog must stay in sync",
    );
}
