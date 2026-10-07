use super::*;
use choreo_proto::ClientMessageType;

#[test]
fn parses_empty_line() {
    assert_eq!(parse_input_line("   "), Command::Empty);
}

#[test]
fn parses_ping() {
    assert_eq!(
        parse_input_line("/ping"),
        Command::Send(ClientMessageType::Ping)
    );
}

#[test]
fn parses_cancel() {
    assert_eq!(
        parse_input_line("/cancel 42"),
        Command::Send(ClientMessageType::Cancel { stream_id: 42 })
    );
}

#[test]
fn rejects_invalid_cancel() {
    assert_eq!(
        parse_input_line("/cancel nope"),
        Command::InvalidCancel("nope".to_string())
    );
}

#[test]
fn parses_unlock_raw() {
    assert_eq!(
        parse_input_line("/unlock"),
        Command::Unlock {
            method: UnlockMethod::Raw,
        }
    );
}

#[test]
fn parses_unlock_with_base64_key() {
    assert_eq!(
        parse_input_line("/unlock aGVsbG8="),
        Command::Unlock {
            method: UnlockMethod::Key("aGVsbG8=".to_string()),
        }
    );
}

#[test]
fn models_command_is_removed() {
    // `/models` was an alias for `/model`; the alias was dropped, so it is no
    // longer recognised (and must NOT be silently accepted).
    assert_eq!(
        parse_input_line("/models"),
        Command::UnknownCommand("unknown command: /models".to_string())
    );
}

#[test]
fn models_command_with_arg_is_removed() {
    assert_eq!(
        parse_input_line("/models gpt-5.4-nano"),
        Command::UnknownCommand("unknown command: /models gpt-5.4-nano".to_string())
    );
}

#[test]
fn model_bare_opens_selector() {
    assert_eq!(parse_input_line("/model"), Command::OpenModelSelector);
}

#[test]
fn model_set() {
    assert_eq!(
        parse_input_line("/model gpt-5.4-nano"),
        Command::Send(ClientMessageType::SetModel {
            model: "gpt-5.4-nano".to_string(),
        })
    );
}

#[test]
fn rejects_unknown_command() {
    assert_eq!(
        parse_input_line("/bogus"),
        Command::UnknownCommand("unknown command: /bogus".to_string())
    );
}

#[test]
fn session_bare_opens_manager() {
    // Bare `/session` opens the session manager.
    assert_eq!(parse_input_line("/session"), Command::OpenSessions);
}

#[test]
fn session_info_parses_id() {
    assert_eq!(
        parse_input_line("/session info 7"),
        Command::Send(ClientMessageType::GetSessionState { session_id: 7 })
    );
}

#[test]
fn session_info_rejects_invalid_id() {
    assert_eq!(
        parse_input_line("/session info nope"),
        Command::UnknownCommand("usage: /session info <id>".to_string())
    );
}

#[test]
fn session_list() {
    assert_eq!(
        parse_input_line("/session list"),
        Command::Send(ClientMessageType::ListSessions)
    );
}

#[test]
fn session_new_with_title() {
    assert_eq!(
        parse_input_line("/session new my title"),
        Command::Send(ClientMessageType::CreateSession {
            title: Some("my title".to_string()),
            parent_session_id: None,
            working_dir: None,
            context_config: None,
            account_name: None,
            selected_model: None,
            reasoning_effort: None,
        })
    );
}

#[test]
fn session_new_without_title() {
    assert_eq!(
        parse_input_line("/session new"),
        Command::Send(ClientMessageType::CreateSession {
            title: None,
            parent_session_id: None,
            working_dir: None,
            context_config: None,
            account_name: None,
            selected_model: None,
            reasoning_effort: None,
        })
    );
}

#[test]
fn session_switch() {
    assert_eq!(
        parse_input_line("/session switch 5"),
        Command::Send(ClientMessageType::AttachSession { session_id: 5 })
    );
}

#[test]
fn new_without_title() {
    // Bare `/new` is the top-level shortcut for `/session new`.
    assert_eq!(
        parse_input_line("/new"),
        Command::Send(ClientMessageType::CreateSession {
            title: None,
            parent_session_id: None,
            working_dir: None,
            context_config: None,
            account_name: None,
            selected_model: None,
            reasoning_effort: None,
        })
    );
}

#[test]
fn new_with_title() {
    assert_eq!(
        parse_input_line("/new my title"),
        Command::Send(ClientMessageType::CreateSession {
            title: Some("my title".to_string()),
            parent_session_id: None,
            working_dir: None,
            context_config: None,
            account_name: None,
            selected_model: None,
            reasoning_effort: None,
        })
    );
}

#[test]
fn new_and_session_new_agree() {
    // `/new [title]` and `/session new [title]` must produce the same command
    // so the two entry points can never drift apart.
    for (bare, grouped) in [("/new", "/session new"), ("/new t", "/session new t")] {
        assert_eq!(
            parse_input_line(bare),
            parse_input_line(grouped),
            "`{bare}` and `{grouped}` must parse identically"
        );
    }
}

#[test]
fn session_switch_rejects_invalid_id() {
    assert_eq!(
        parse_input_line("/session switch nope"),
        Command::UnknownCommand("usage: /session switch <id>".to_string())
    );
}

#[test]
fn session_unknown_subcommand() {
    assert_eq!(
        parse_input_line("/session bogus"),
        Command::UnknownCommand(
            "session subcommands: list, new [title], switch <id>, info <id>".to_string()
        )
    );
}

#[test]
fn parses_add_key() {
    assert_eq!(
        parse_input_line("/add-key openai sk-abc123"),
        Command::AddCredential {
            service: "openai".to_string(),
            credential_type: "api_key".to_string(),
            fields: vec!["sk-abc123".to_string()],
        }
    );
}

#[test]
fn ignores_trailing_unlock_arg_for_add_key() {
    // The `[unlock]` argument was removed with the per-daemon unlock-key
    // design: key resolution is per-addr inside build_add_credential_message.
    assert_eq!(
        parse_input_line("/add-key openai sk-abc123 unlock"),
        Command::AddCredential {
            service: "openai".to_string(),
            credential_type: "api_key".to_string(),
            fields: vec!["sk-abc123".to_string()],
        }
    );
}

#[test]
fn rejects_add_key_without_enough_args() {
    assert_eq!(
        parse_input_line("/add-key openai"),
        Command::UnknownCommand("usage: /add-key <service> <api_key>".to_string())
    );
}

#[test]
fn parses_add_x() {
    assert_eq!(
        parse_input_line("/add-x twitter ck cs at ats -"),
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
}

#[test]
fn parses_add_x_with_bearer() {
    assert_eq!(
        parse_input_line("/add-x twitter ck cs at ats mybearer"),
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
}

#[test]
fn rejects_add_x_without_enough_args() {
    assert_eq!(
        parse_input_line("/add-x twitter ck cs"),
        Command::UnknownCommand("usage: /add-x <service> <api_key> <api_key_secret> <access_token> <access_token_secret> <bearer_or_->_".to_string())
    );
}

#[test]
fn parses_remove_key() {
    assert_eq!(
        parse_input_line("/remove-key openai"),
        Command::RemoveCredential {
            service: "openai".to_string(),
        }
    );
}

#[test]
fn parses_acl_add_with_valid_key() {
    // b64 of exactly 32 bytes.
    let key_b64 = "AQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyA=";
    assert_eq!(
        parse_input_line(&format!("/acl add {key_b64}")),
        Command::AclAdd {
            pubkey: key_b64.to_string(),
        }
    );
}

#[test]
fn rejects_acl_add_with_bad_base64_or_wrong_length() {
    assert!(matches!(
        parse_input_line("/acl add not-base64!!!"),
        Command::UnknownCommand(_)
    ));
    // Valid base64 but 16 bytes.
    assert!(matches!(
        parse_input_line("/acl add c29tZTE2Ynl0ZXNr"),
        Command::UnknownCommand(_)
    ));
    // Missing argument.
    assert!(matches!(
        parse_input_line("/acl add"),
        Command::UnknownCommand(_)
    ));
}

#[test]
fn rejects_remove_key_with_invalid_service_name() {
    assert_eq!(
        parse_input_line("/remove-key my service"),
        Command::UnknownCommand(
            "account name must be lowercase alphanumeric, hyphens, or underscores".to_string()
        )
    );
}

#[test]
fn rejects_add_key_with_invalid_service_name() {
    assert_eq!(
        parse_input_line("/add-key Jonathan sk-test"),
        Command::UnknownCommand(
            "account name must be lowercase alphanumeric, hyphens, or underscores".to_string()
        )
    );
}

#[test]
fn rejects_account_set_with_invalid_name() {
    assert_eq!(
        parse_input_line("/account Jonathan's Opencode"),
        Command::UnknownCommand(
            "account name must be lowercase alphanumeric, hyphens, or underscores".to_string()
        )
    );
}

#[test]
fn rejects_remove_key_without_service() {
    assert_eq!(
        parse_input_line("/remove-key"),
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
fn parses_run_input() {
    // The run carries only its input; the daemon assigns the `stream_id`.
    assert_eq!(
        parse_input_line("hello world"),
        Command::Send(ClientMessageType::RunInput {
            input: b"hello world".to_vec(),
        })
    );
}

// ── Account sub-commands ──────────────────────────────────────────────────

#[test]
fn account_list() {
    assert_eq!(
        parse_input_line("/account list"),
        Command::Send(ClientMessageType::ListAccounts)
    );
}

#[test]
fn account_bare_opens_page() {
    assert_eq!(parse_input_line("/account"), Command::OpenAccounts);
}

#[test]
fn account_remove() {
    assert_eq!(
        parse_input_line("/account remove my-provider"),
        Command::Send(ClientMessageType::RemoveAccount {
            name: "my-provider".to_string()
        })
    );
}

#[test]
fn account_remove_missing_name() {
    assert_eq!(
        parse_input_line("/account remove"),
        Command::UnknownCommand("usage: /account remove <name>".to_string())
    );
}

#[test]
fn account_set_valid_name() {
    assert_eq!(
        parse_input_line("/account my-account"),
        Command::Send(ClientMessageType::SetSessionAccount {
            name: "my-account".to_string()
        })
    );
}

// ── Reasoning effort ──────────────────────────────────────────────────────

#[test]
fn reasoning_bare_cycles() {
    assert_eq!(parse_input_line("/reasoning"), Command::ReasoningCycle);
}

#[test]
fn reasoning_list() {
    assert_eq!(parse_input_line("/reasoning list"), Command::ReasoningList);
}

#[test]
fn reasoning_set_off() {
    assert_eq!(
        parse_input_line("/reasoning off"),
        Command::Send(ClientMessageType::SetReasoningEffort {
            effort: "off".to_string()
        })
    );
}

#[test]
fn reasoning_set_low() {
    assert_eq!(
        parse_input_line("/reasoning low"),
        Command::Send(ClientMessageType::SetReasoningEffort {
            effort: "low".to_string()
        })
    );
}

#[test]
fn reasoning_set_medium() {
    assert_eq!(
        parse_input_line("/reasoning medium"),
        Command::Send(ClientMessageType::SetReasoningEffort {
            effort: "medium".to_string()
        })
    );
}

#[test]
fn reasoning_set_high() {
    assert_eq!(
        parse_input_line("/reasoning high"),
        Command::Send(ClientMessageType::SetReasoningEffort {
            effort: "high".to_string()
        })
    );
}

#[test]
fn reasoning_set_none_alias() {
    assert_eq!(
        parse_input_line("/reasoning none"),
        Command::Send(ClientMessageType::SetReasoningEffort {
            effort: "off".to_string()
        })
    );
}

#[test]
fn reasoning_set_disabled_alias() {
    assert_eq!(
        parse_input_line("/reasoning disabled"),
        Command::Send(ClientMessageType::SetReasoningEffort {
            effort: "off".to_string()
        })
    );
}

#[test]
fn reasoning_set_med_alias() {
    assert_eq!(
        parse_input_line("/reasoning med"),
        Command::Send(ClientMessageType::SetReasoningEffort {
            effort: "medium".to_string()
        })
    );
}

#[test]
fn reasoning_set_on() {
    assert_eq!(
        parse_input_line("/reasoning on"),
        Command::Send(ClientMessageType::SetReasoningEffort {
            effort: "on".to_string()
        })
    );
}

#[test]
fn reasoning_unknown_slug_passes_through() {
    assert_eq!(
        parse_input_line("/reasoning turbo"),
        Command::Send(ClientMessageType::SetReasoningEffort {
            effort: "turbo".to_string()
        })
    );
}

#[test]
fn reasoning_max_slug_passes_through() {
    assert_eq!(
        parse_input_line("/reasoning max"),
        Command::Send(ClientMessageType::SetReasoningEffort {
            effort: "max".to_string()
        })
    );
}

// ── Undo / Redo commands ────────────────────────────────────────────────

#[test]
fn parses_undo() {
    assert_eq!(parse_input_line("/undo"), Command::Undo);
}

#[test]
fn parses_redo() {
    assert_eq!(parse_input_line("/redo"), Command::Redo);
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
    assert_eq!(parse_input_line("/continue"), Command::Continue);
}

#[test]
fn parses_stop() {
    assert_eq!(parse_input_line("/stop"), Command::Stop);
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
    assert_eq!(parse_input_line("/quit"), Command::Quit);
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
    "mcp",
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
        let cmd = parse_input_line(&line);
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
