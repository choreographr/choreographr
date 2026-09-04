use super::*;
use choreo_proto::ClientMessage;

#[test]
fn parses_empty_line() {
    let mut next = 1;
    assert_eq!(
        parse_input_line("   ", &mut next, None),
        ShellCommand::Empty
    );
    assert_eq!(next, 1);
}

#[test]
fn parses_ping() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/ping", &mut next, None),
        ShellCommand::Send(ClientMessage::Ping)
    );
    assert_eq!(next, 3);
}

#[test]
fn parses_cancel() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/cancel 42", &mut next, None),
        ShellCommand::Send(ClientMessage::Cancel { request_id: 42 })
    );
    assert_eq!(next, 3);
}

#[test]
fn rejects_invalid_cancel() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/cancel nope", &mut next, None),
        ShellCommand::InvalidCancel("nope".to_string())
    );
    assert_eq!(next, 3);
}

#[test]
fn parses_unlock_raw() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/unlock", &mut next, None),
        ShellCommand::Unlock {
            method: UnlockMethod::Raw,
        }
    );
    assert_eq!(next, 3);
}

#[test]
fn parses_unlock_with_base64_key() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/unlock aGVsbG8=", &mut next, None),
        ShellCommand::Unlock {
            method: UnlockMethod::Key("aGVsbG8=".to_string()),
        }
    );
    assert_eq!(next, 3);
}

#[test]
fn parses_models_command() {
    let mut next = 10;
    assert_eq!(
        parse_input_line("/models", &mut next, None),
        ShellCommand::Send(ClientMessage::ListModels)
    );
    assert_eq!(next, 10);
}

#[test]
fn parses_set_model_command() {
    let mut next = 10;
    assert_eq!(
        parse_input_line("/models gpt-5.4-nano", &mut next, None),
        ShellCommand::Send(ClientMessage::SetModel {
            model: "gpt-5.4-nano".to_string(),
        })
    );
    assert_eq!(next, 10);
}

#[test]
fn parses_model_alias_list() {
    let mut next = 10;
    assert_eq!(
        parse_input_line("/model", &mut next, None),
        ShellCommand::Send(ClientMessage::ListModels)
    );
    assert_eq!(next, 10);
}

#[test]
fn parses_model_alias_set() {
    let mut next = 10;
    assert_eq!(
        parse_input_line("/model gpt-5.4-nano", &mut next, None),
        ShellCommand::Send(ClientMessage::SetModel {
            model: "gpt-5.4-nano".to_string(),
        })
    );
    assert_eq!(next, 10);
}

#[test]
fn rejects_unknown_command() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/bogus", &mut next, None),
        ShellCommand::UnknownCommand("unknown command: /bogus".to_string())
    );
    assert_eq!(next, 3);
}

#[test]
fn session_without_args_uses_attached_session_id() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/session", &mut next, Some(42)),
        ShellCommand::Send(ClientMessage::GetSessionState { session_id: 42 })
    );
    assert_eq!(next, 3);
}

#[test]
fn session_without_args_fails_when_no_attached_session() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/session", &mut next, None),
        ShellCommand::UnknownCommand(
            "no session attached. use /session switch <id> to attach".to_string()
        )
    );
    assert_eq!(next, 3);
}

#[test]
fn session_info_parses_id() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/session info 7", &mut next, None),
        ShellCommand::Send(ClientMessage::GetSessionState { session_id: 7 })
    );
    assert_eq!(next, 3);
}

#[test]
fn session_info_rejects_invalid_id() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/session info nope", &mut next, None),
        ShellCommand::UnknownCommand("usage: /session info <id>".to_string())
    );
    assert_eq!(next, 3);
}

#[test]
fn session_list() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/session list", &mut next, None),
        ShellCommand::Send(ClientMessage::ListSessions)
    );
    assert_eq!(next, 3);
}

#[test]
fn session_new_with_title() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/session new my title", &mut next, None),
        ShellCommand::Send(ClientMessage::CreateSession {
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
        parse_input_line("/session new", &mut next, None),
        ShellCommand::Send(ClientMessage::CreateSession {
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
        parse_input_line("/session switch 5", &mut next, None),
        ShellCommand::Send(ClientMessage::AttachSession { session_id: 5 })
    );
    assert_eq!(next, 3);
}

#[test]
fn session_switch_rejects_invalid_id() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/session switch nope", &mut next, None),
        ShellCommand::UnknownCommand("usage: /session switch <id>".to_string())
    );
    assert_eq!(next, 3);
}

#[test]
fn session_unknown_subcommand() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/session bogus", &mut next, None),
        ShellCommand::UnknownCommand(
            "session subcommands: list, new [title], switch <id>, info <id>".to_string()
        )
    );
    assert_eq!(next, 3);
}

#[test]
fn parses_add_key() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/add-key openai sk-abc123", &mut next, None),
        ShellCommand::AddCredential {
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
        parse_input_line("/add-key openai sk-abc123 unlock", &mut next, None),
        ShellCommand::AddCredential {
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
        parse_input_line("/add-key openai", &mut next, None),
        ShellCommand::UnknownCommand("usage: /add-key <service> <api_key>".to_string())
    );
    assert_eq!(next, 3);
}

#[test]
fn parses_add_x() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/add-x twitter ck cs at ats -", &mut next, None),
        ShellCommand::AddCredential {
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
        parse_input_line("/add-x twitter ck cs at ats mybearer", &mut next, None),
        ShellCommand::AddCredential {
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
        parse_input_line("/add-x twitter ck cs", &mut next, None),
        ShellCommand::UnknownCommand("usage: /add-x <service> <api_key> <api_key_secret> <access_token> <access_token_secret> <bearer_or_->_".to_string())
    );
    assert_eq!(next, 3);
}

#[test]
fn parses_remove_key() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/remove-key openai", &mut next, None),
        ShellCommand::RemoveCredential {
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
        parse_input_line(&format!("/acl add {key_b64}"), &mut next, None),
        ShellCommand::AclAdd {
            pubkey: key_b64.to_string(),
        }
    );
    assert_eq!(next, 3);
}

#[test]
fn rejects_acl_add_with_bad_base64_or_wrong_length() {
    let mut next = 3;
    assert!(matches!(
        parse_input_line("/acl add not-base64!!!", &mut next, None),
        ShellCommand::UnknownCommand(_)
    ));
    // Valid base64 but 16 bytes.
    assert!(matches!(
        parse_input_line("/acl add c29tZTE2Ynl0ZXNr", &mut next, None),
        ShellCommand::UnknownCommand(_)
    ));
    // Missing argument.
    assert!(matches!(
        parse_input_line("/acl add", &mut next, None),
        ShellCommand::UnknownCommand(_)
    ));
}

#[test]
fn rejects_remove_key_with_invalid_service_name() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/remove-key my service", &mut next, None),
        ShellCommand::UnknownCommand(
            "account name must be lowercase alphanumeric, hyphens, or underscores".to_string()
        )
    );
}

#[test]
fn rejects_add_key_with_invalid_service_name() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/add-key Jonathan sk-test", &mut next, None),
        ShellCommand::UnknownCommand(
            "account name must be lowercase alphanumeric, hyphens, or underscores".to_string()
        )
    );
}

#[test]
fn rejects_account_set_with_invalid_name() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/account Jonathan's Opencode", &mut next, None),
        ShellCommand::UnknownCommand(
            "account name must be lowercase alphanumeric, hyphens, or underscores".to_string()
        )
    );
}

#[test]
fn rejects_remove_key_without_service() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/remove-key", &mut next, None),
        ShellCommand::UnknownCommand("usage: /remove-key <service>".to_string())
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
        parse_input_line("hello world", &mut next, None),
        ShellCommand::Send(ClientMessage::RunInput {
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
        parse_input_line("/account list", &mut next, None),
        ShellCommand::Send(ClientMessage::ListAccounts)
    );
    assert_eq!(next, 3);
}

#[test]
fn account_bare_lists_accounts() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/account", &mut next, None),
        ShellCommand::Send(ClientMessage::ListAccounts)
    );
    assert_eq!(next, 3);
}

#[test]
fn account_remove() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/account remove my-provider", &mut next, None),
        ShellCommand::Send(ClientMessage::RemoveAccount {
            name: "my-provider".to_string()
        })
    );
    assert_eq!(next, 3);
}

#[test]
fn account_remove_missing_name() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/account remove", &mut next, None),
        ShellCommand::UnknownCommand("usage: /account remove <name>".to_string())
    );
    assert_eq!(next, 3);
}

#[test]
fn account_set_valid_name() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/account my-account", &mut next, None),
        ShellCommand::Send(ClientMessage::SetSessionAccount {
            name: "my-account".to_string()
        })
    );
    assert_eq!(next, 3);
}

// ── Reasoning effort ──────────────────────────────────────────────────────

#[test]
fn reasoning_get() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/reasoning", &mut next, None),
        ShellCommand::Send(ClientMessage::GetReasoningEffort)
    );
    assert_eq!(next, 3);
}

#[test]
fn reasoning_set_off() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/reasoning off", &mut next, None),
        ShellCommand::Send(ClientMessage::SetReasoningEffort {
            effort: "off".to_string()
        })
    );
    assert_eq!(next, 3);
}

#[test]
fn reasoning_set_low() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/reasoning low", &mut next, None),
        ShellCommand::Send(ClientMessage::SetReasoningEffort {
            effort: "low".to_string()
        })
    );
    assert_eq!(next, 3);
}

#[test]
fn reasoning_set_medium() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/reasoning medium", &mut next, None),
        ShellCommand::Send(ClientMessage::SetReasoningEffort {
            effort: "medium".to_string()
        })
    );
    assert_eq!(next, 3);
}

#[test]
fn reasoning_set_high() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/reasoning high", &mut next, None),
        ShellCommand::Send(ClientMessage::SetReasoningEffort {
            effort: "high".to_string()
        })
    );
    assert_eq!(next, 3);
}

#[test]
fn reasoning_set_none_alias() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/reasoning none", &mut next, None),
        ShellCommand::Send(ClientMessage::SetReasoningEffort {
            effort: "off".to_string()
        })
    );
    assert_eq!(next, 3);
}

#[test]
fn reasoning_set_disabled_alias() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/reasoning disabled", &mut next, None),
        ShellCommand::Send(ClientMessage::SetReasoningEffort {
            effort: "off".to_string()
        })
    );
    assert_eq!(next, 3);
}

#[test]
fn reasoning_set_med_alias() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/reasoning med", &mut next, None),
        ShellCommand::Send(ClientMessage::SetReasoningEffort {
            effort: "medium".to_string()
        })
    );
    assert_eq!(next, 3);
}

#[test]
fn reasoning_set_on() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/reasoning on", &mut next, None),
        ShellCommand::Send(ClientMessage::SetReasoningEffort {
            effort: "on".to_string()
        })
    );
    assert_eq!(next, 3);
}

#[test]
fn reasoning_unknown_slug_passes_through() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/reasoning turbo", &mut next, None),
        ShellCommand::Send(ClientMessage::SetReasoningEffort {
            effort: "turbo".to_string()
        })
    );
    assert_eq!(next, 3);
}

#[test]
fn reasoning_max_slug_passes_through() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/reasoning max", &mut next, None),
        ShellCommand::Send(ClientMessage::SetReasoningEffort {
            effort: "max".to_string()
        })
    );
    assert_eq!(next, 3);
}

// ── Undo / Redo commands ────────────────────────────────────────────────

#[test]
fn parses_undo() {
    let mut next = 5;
    assert_eq!(
        parse_input_line("/undo", &mut next, None),
        ShellCommand::Undo
    );
    assert_eq!(next, 5);
}

#[test]
fn parses_redo() {
    let mut next = 5;
    assert_eq!(
        parse_input_line("/redo", &mut next, None),
        ShellCommand::Redo
    );
    assert_eq!(next, 5);
}

#[test]
fn shell_command_echo_undo() {
    assert_eq!(
        shell_command_echo(&ShellCommand::Undo),
        Some("> undo".to_string())
    );
}

#[test]
fn shell_command_echo_redo() {
    assert_eq!(
        shell_command_echo(&ShellCommand::Redo),
        Some("> redo".to_string())
    );
}

// ── Continue / Stop commands ────────────────────────────────────────────

#[test]
fn parses_continue() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/continue", &mut next, None),
        ShellCommand::Continue
    );
    assert_eq!(next, 3);
}

#[test]
fn parses_stop() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/stop", &mut next, None),
        ShellCommand::Stop
    );
    assert_eq!(next, 3);
}

#[test]
fn shell_command_continue_echo() {
    assert_eq!(
        shell_command_echo(&ShellCommand::Continue),
        Some("> continue".to_string())
    );
}

#[test]
fn shell_command_stop_echo() {
    assert_eq!(
        shell_command_echo(&ShellCommand::Stop),
        Some("> stop".to_string())
    );
}
