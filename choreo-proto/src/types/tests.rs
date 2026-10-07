use super::*;

#[test]
fn session_status_idle_and_active_partition_busy_states() {
    // `is_idle` is exactly `Inactive`; `is_active` covers the three
    // processing states.  `Sleeping` is neither — it is the exit marker of
    // a session whose thread has terminated, so it cannot begin a turn
    // (hence not idle) yet is not "actively processing" either (hence not
    // active).  Both predicates must agree on this partition.
    assert!(SessionStatus::Inactive.is_idle());
    assert!(!SessionStatus::Inactive.is_active());

    for busy in [
        SessionStatus::Inference,
        SessionStatus::ToolCall("shell".into()),
        SessionStatus::Retrying {
            attempt: 2,
            max_attempts: 5,
            delay_ms: 250,
        },
    ] {
        assert!(busy.is_active(), "{busy:?} must be active");
        assert!(!busy.is_idle(), "{busy:?} must not be idle");
    }

    assert!(!SessionStatus::Sleeping.is_idle());
    assert!(!SessionStatus::Sleeping.is_active());
}

/// All four artifact variants, with realistic payload bytes.
fn all_artifacts() -> Vec<ReasoningArtifact> {
    vec![
            ReasoningArtifact::ChatReasoning {
                field: ChatReasoningField::ReasoningContent,
                bytes: b"deep think step-by-step".to_vec(),
            },
            ReasoningArtifact::AnthropicThinking(
                br#"[{"type":"thinking","thinking":"...","signature":"sig_abc"},{"type":"redacted_thinking","data":"eJxT"}]"#
                    .to_vec(),
            ),
            ReasoningArtifact::GoogleSignatures(b"encrypted-sig-1\nencrypted-sig-2".to_vec()),
            ReasoningArtifact::ResponsesItems(b"[{\"type\":\"reasoning\",\"id\":\"re_1\"}]".to_vec()),
        ]
}

#[test]
fn reasoning_artifact_variants_round_trip_msgpack() {
    // Named MessagePack is the workspace wire format (see frame.rs) —
    // persistence and the client socket both round-trip through it, so
    // every variant must survive byte-for-byte.
    for artifact in all_artifacts() {
        let bytes = rmp_serde::to_vec_named(&artifact).expect("encode");
        let decoded: ReasoningArtifact = rmp_serde::from_slice(&bytes).expect("decode");
        assert_eq!(decoded, artifact);
    }
}

#[test]
fn reasoning_artifact_variants_round_trip_json() {
    // serde_json is a dev-dependency already; the externally-tagged serde
    // layout (variant name as the object key, payload as its value) must
    // round-trip too.
    for artifact in all_artifacts() {
        let json = serde_json::to_string(&artifact).expect("serialize");
        let decoded: ReasoningArtifact = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, artifact);
    }
}

#[test]
fn reasoning_artifact_json_uses_kind_tag() {
    // The variant name is the adapter-ownership contract: the producing
    // adapter's identity must be visible on the wire (as the JSON object
    // key) without interpreting the payload bytes. Pin the exact shape so
    // a serde refactor cannot silently change it.
    let artifact = ReasoningArtifact::ChatReasoning {
        field: ChatReasoningField::ReasoningContent,
        bytes: b"hi".to_vec(),
    };
    let json = serde_json::to_value(&artifact).expect("serialize");
    assert_eq!(
        json["chat_reasoning"]["field"],
        serde_json::json!("reasoning_content")
    );
    assert_eq!(
        json["chat_reasoning"]["bytes"],
        serde_json::json!([104, 105])
    );
    // Exactly one key — the ownership tag — and nothing else.
    let keys: Vec<_> = json.as_object().expect("object").keys().collect();
    assert_eq!(keys, vec!["chat_reasoning"]);
}

#[test]
fn chat_reasoning_struct_variant_round_trips_msgpack_and_json() {
    // The struct-variant ChatReasoning (field + bytes) is the one variant
    // whose payload is not a bare byte array — pin both wire formats so a
    // serde/codec refactor cannot silently drop the field identity
    // (re-emission would then mis-route to the default reasoning_content).
    for artifact in [
        ReasoningArtifact::ChatReasoning {
            field: ChatReasoningField::ReasoningContent,
            bytes: b"deep think step-by-step".to_vec(),
        },
        ReasoningArtifact::ChatReasoning {
            field: ChatReasoningField::Reasoning,
            bytes: b"bare reasoning".to_vec(),
        },
        ReasoningArtifact::ChatReasoning {
            field: ChatReasoningField::ReasoningText,
            bytes: b"text reasoning".to_vec(),
        },
    ] {
        let bytes = rmp_serde::to_vec_named(&artifact).expect("encode");
        let decoded: ReasoningArtifact = rmp_serde::from_slice(&bytes).expect("decode");
        assert_eq!(
            decoded, artifact,
            "MessagePack round-trip must keep field + bytes"
        );

        let json = serde_json::to_string(&artifact).expect("serialize");
        let decoded: ReasoningArtifact = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, artifact, "JSON round-trip must keep field + bytes");
    }
}

#[test]
fn reasoning_producer_round_trip_msgpack() {
    let producer = ReasoningProducer {
        provider_slug: "openai".to_string(),
        model: "gpt-5.6".to_string(),
    };
    let bytes = rmp_serde::to_vec_named(&producer).expect("encode");
    let decoded: ReasoningProducer = rmp_serde::from_slice(&bytes).expect("decode");
    assert_eq!(decoded, producer);
}

/// A fully-populated Turn used by the round-trip tests below.
fn sample_turn(
    reasoning_artifact: Option<ReasoningArtifact>,
    reasoning_producer: Option<ReasoningProducer>,
) -> Turn {
    Turn {
        created_at: TimestampMs(1_700_000_000_000),
        undone: false,
        error: None,
        user_text: Some("list files".to_string()),
        assistant_text: None,
        assistant_reasoning: Some("thinking…".to_string()),
        tool_calls: vec![AssistantToolCallRecord {
            call_id: "call_1".to_string(),
            name: "ls".to_string(),
            arguments_json: "{}".to_string(),
        }],
        token_usage: Some(TokenUsage {
            input_tokens: 10,
            output_tokens: 20,
            total_tokens: 30,
            ..Default::default()
        }),
        tool_results: vec![ToolResultRecord {
            call_id: "call_1".to_string(),
            name: "ls".to_string(),
            content: "file.txt".to_string(),
            is_error: false,
            invocation_description: String::new(),
            image: None,
        }],
        displayed_images: vec![DisplayedImageRecord {
            metadata: ImageMetadata {
                mime_type: "image/png".to_string(),
                width: 640,
                height: 480,
                byte_len: 100,
                alt: None,
            },
            data: vec![0u8; 100],
            tool_call_id: None,
        }],
        reasoning_artifact,
        reasoning_producer,
    }
}

#[test]
fn turn_with_artifact_and_producer_round_trips_msgpack() {
    let turn = sample_turn(
        Some(ReasoningArtifact::AnthropicThinking(
            b"{\"sig\":\"x\"}".to_vec(),
        )),
        Some(ReasoningProducer {
            provider_slug: "anthropic".to_string(),
            model: "claude-4.6".to_string(),
        }),
    );
    let bytes = rmp_serde::to_vec_named(&turn).expect("encode");
    let decoded: Turn = rmp_serde::from_slice(&bytes).expect("decode");
    assert_eq!(decoded, turn);
}

#[test]
fn turn_without_artifact_round_trips_msgpack() {
    // Legacy/placeholder turns (and providers that expose no reusable
    // artifact) must round-trip with both new fields as None.
    let turn = sample_turn(None, None);
    let bytes = rmp_serde::to_vec_named(&turn).expect("encode");
    let decoded: Turn = rmp_serde::from_slice(&bytes).expect("decode");
    assert_eq!(decoded, turn);
    assert!(decoded.reasoning_artifact.is_none());
    assert!(decoded.reasoning_producer.is_none());
}

#[test]
fn add_credential_round_trips_with_required_unlock_key() {
    // unlock_key is now REQUIRED (per-daemon keystore TOFU design): the
    // client encrypts the blob with the derived pubkey, so the daemon
    // cannot do anything useful without the key. Pin both wire formats
    // and assert the key survives byte-for-byte so a future serde change
    // cannot silently make it optional or drop it.
    let msg = ClientMessage::request(
        7,
        ClientMessageType::AddCredential {
            service: "openai".to_string(),
            encrypted_payload: vec![1u8, 2, 3, 4, 5],
            unlock_key: vec![9u8; 32],
        },
    );

    let bytes = rmp_serde::to_vec_named(&msg).expect("encode");
    let decoded: ClientMessage = rmp_serde::from_slice(&bytes).expect("decode");
    assert_eq!(decoded, msg, "MessagePack round-trip must keep the key");

    let json = serde_json::to_string(&msg).expect("serialize");
    let decoded: ClientMessage = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(decoded, msg, "JSON round-trip must keep the key");
}

#[test]
fn add_credential_without_unlock_key_fails_to_decode() {
    // The old wire shape carried an absent/None key; the new required
    // field must REJECT such payloads rather than silently decoding with
    // an empty key (postcard/named-msgpack would misinterpret a missing
    // field). Pin the rejection so the requirement is enforced on the
    // wire, not just in the type system.
    let bytes = rmp_serde::to_vec_named(&serde_json::json!({
        "id": 0,
        "inner": {
            "AddCredential": {
                "service": "openai",
                "encrypted_payload": [1, 2, 3]
            }
        }
    }))
    .expect("encode legacy-shaped payload");
    let result: Result<ClientMessage, _> = rmp_serde::from_slice(&bytes);
    assert!(result.is_err(), "missing unlock_key must not decode");
}

#[test]
fn token_usage_merge_max_keeps_most_advanced_field_per_field() {
    // Cumulative usage only ever increases, so the merge is a per-field
    // max — never regressing any counter even when one side trails the
    // other on a subset of fields (an attach snapshot built before the
    // mid-turn sync landed).
    let mut usage = TokenUsage {
        input_tokens: 30,
        output_tokens: 5,
        total_tokens: 35,
        cached_tokens: 12,
        cache_write_tokens: 8,
    };
    usage.merge_max(TokenUsage {
        input_tokens: 10,
        output_tokens: 15,
        total_tokens: 25,
        cached_tokens: 20,
        cache_write_tokens: 3,
    });
    assert_eq!(usage.input_tokens, 30);
    assert_eq!(usage.output_tokens, 15);
    assert_eq!(usage.total_tokens, 35);
    assert_eq!(usage.cached_tokens, 20);
    assert_eq!(usage.cache_write_tokens, 8);

    // An identical or trailing value is a no-op.
    usage.merge_max(TokenUsage {
        input_tokens: 30,
        output_tokens: 15,
        total_tokens: 35,
        cached_tokens: 18,
        cache_write_tokens: 5,
    });
    assert_eq!(
        usage,
        TokenUsage {
            input_tokens: 30,
            output_tokens: 15,
            total_tokens: 35,
            cached_tokens: 20,
            cache_write_tokens: 8,
        }
    );
}

/// The lag-eviction gauge must never UNDER-estimate the serialized payload,
/// or a genuinely lagging client could escape eviction: the estimate is the
/// threshold the daemon's lag accounting compares against the per-client
/// cap / global budget, so under-counting directly weakens the memory
/// bound. Every `DaemonMessage` variant is encoded with a realistic (and
/// deliberately DENSE for the record-bearing ones) payload, and the
/// estimate must cover the actual frame bytes. This is the property the
/// record-size allowances in [`Turn::approx_size`] and the per-variant
/// field counts are tuned against — a future serde/encoding change or a
/// new variant that shrinks the margin must re-prove it here.
#[test]
fn approx_wire_size_never_underestimates_encoded_payload() {
    fn turn(n_calls: usize, n_results: usize, n_images: usize) -> Turn {
        Turn {
            created_at: TimestampMs(1_700_000_000_000),
            undone: false,
            error: Some("boom".into()),
            user_text: Some("hello world".into()),
            assistant_text: Some("x".repeat(100)),
            assistant_reasoning: Some("thinking".repeat(10)),
            tool_calls: (0..n_calls)
                .map(|i| AssistantToolCallRecord {
                    call_id: format!("call_{i}"),
                    name: "sh".into(),
                    arguments_json: format!(r#"{{"command":"echo step {i}"}}"#),
                })
                .collect(),
            token_usage: Some(TokenUsage {
                input_tokens: 1,
                output_tokens: 2,
                total_tokens: 3,
                ..Default::default()
            }),
            tool_results: (0..n_results)
                .map(|i| ToolResultRecord {
                    call_id: format!("call_{i}"),
                    name: "sh".into(),
                    content: format!("output line {i} of the tool\n"),
                    is_error: false,
                    invocation_description: format!("Running: echo step {i}"),
                    image: None,
                })
                .collect(),
            displayed_images: (0..n_images)
                .map(|i| DisplayedImageRecord {
                    metadata: ImageMetadata {
                        mime_type: "image/png".into(),
                        width: 640,
                        height: 480,
                        byte_len: 100,
                        alt: Some(format!("screenshot {i}")),
                    },
                    data: vec![0u8; 100],
                    tool_call_id: Some(format!("call_{i}")),
                })
                .collect(),
            reasoning_artifact: Some(ReasoningArtifact::ChatReasoning {
                field: ChatReasoningField::ReasoningContent,
                bytes: b"{\"type\":\"thinking\",\"signature\":\"sig_abc\"}".to_vec(),
            }),
            reasoning_producer: Some(ReasoningProducer {
                provider_slug: "anthropic".into(),
                model: "claude-4.6".into(),
            }),
        }
    }

    fn summary(id: u64) -> SessionSummary {
        SessionSummary {
            session_id: id,
            title: Some(format!("session {id}")),
            selected_model: Some("gpt-5.6".into()),
            reasoning_effort: Some("high".into()),
            parent_session_id: Some(3),
            working_dir: Some("/home/user/projects/demo".into()),
            created_at: 1_700_000_000_000,
            last_modified: 1_700_000_000_001,
            turn_count: 12,
            status: SessionStatus::ToolCall("sh".into()),
            active_tool_groups: vec![
                "core".into(),
                "git".into(),
                "shell".into(),
                "filesystem".into(),
            ],
            account_name: Some("default".into()),
            token_usage: Some(TokenUsage {
                input_tokens: 100,
                output_tokens: 50,
                total_tokens: 150,
                ..Default::default()
            }),
            context_window: Some(128_000),
            last_prompt_tokens: Some(100),
            pinned: id.is_multiple_of(2),
            archived_at: id.is_multiple_of(3).then_some(1_700_000_000_000),
        }
    }

    let dense_turn = turn(10, 10, 3);
    let one_turn = turn(1, 1, 0);
    // A second artifact shape: bare-byte variants (Anthropic/Google/Responses)
    // have a cheaper wire form than `ChatReasoning`, but must still fit.
    let mut bare_artifact_turn = turn(10, 10, 3);
    bare_artifact_turn.reasoning_artifact = Some(ReasoningArtifact::ResponsesItems(
        b"[{\"type\":\"reasoning\",\"id\":\"re_1\"}]".to_vec(),
    ));
    let samples: Vec<(&str, DaemonMessageType)> = vec![
        (
            "SessionCreated",
            DaemonMessageType::Session {
                session_id: Some(1),
                event: SessionEvent::SessionCreated {
                    title: Some("t".into()),
                    parent_session_id: Some(2),
                    working_dir: Some("/tmp".into()),
                    account_name: Some("default".into()),
                    selected_model: Some("gpt-5.6".into()),
                    reasoning_effort: Some("high".into()),
                },
            },
        ),
        (
            "SessionCreatedForRequester",
            DaemonMessageType::Session {
                session_id: Some(1),
                event: SessionEvent::SessionCreatedForRequester {
                    title: Some("t".into()),
                    parent_session_id: None,
                    working_dir: Some("/tmp".into()),
                    account_name: Some("default".into()),
                    selected_model: Some("gpt-5.6".into()),
                    reasoning_effort: Some("high".into()),
                },
            },
        ),
        (
            "Sessions",
            DaemonMessageType::Sessions {
                sessions: (0..20).map(summary).collect(),
            },
        ),
        (
            "SessionAttached",
            DaemonMessageType::Session {
                session_id: Some(1),
                event: SessionEvent::SessionAttached,
            },
        ),
        (
            "SessionFlagsChanged",
            DaemonMessageType::Session {
                session_id: Some(1),
                event: SessionEvent::SessionFlagsChanged {
                    pinned: true,
                    archived_at: Some(1_700_000_000_000),
                },
            },
        ),
        (
            "SessionState",
            DaemonMessageType::Session {
                session_id: Some(1),
                event: SessionEvent::SessionState {
                    title: Some("t".into()),
                    selected_model: Some("gpt-5.6".into()),
                    parent_session_id: Some(2),
                    working_dir: Some("/tmp".into()),
                    turns: std::collections::BTreeMap::from([
                        (1, dense_turn.clone()),
                        (2, one_turn.clone()),
                    ]),
                    active_tool_groups: vec!["core".into(), "shell".into(), "git".into()],
                    token_usage: Some(TokenUsage {
                        input_tokens: 100,
                        output_tokens: 50,
                        total_tokens: 150,
                        ..Default::default()
                    }),
                    context_window: Some(128_000),
                    last_prompt_tokens: Some(100),
                    status: SessionStatus::Inference,
                    reasoning_effort: Some("high".into()),
                    reasoning_capability: Some(ReasoningCapability {
                        available_effort_levels: vec![
                            "off".into(),
                            "low".into(),
                            "medium".into(),
                            "high".into(),
                        ],
                    }),
                },
            },
        ),
        (
            "TurnAppended",
            DaemonMessageType::Session {
                session_id: Some(1),
                event: SessionEvent::TurnAppended {
                    turn_id: 1,
                    turn: dense_turn.clone(),
                },
            },
        ),
        (
            "TurnAppendedBareArtifact",
            DaemonMessageType::Session {
                session_id: Some(1),
                event: SessionEvent::TurnAppended {
                    turn_id: 2,
                    turn: bare_artifact_turn.clone(),
                },
            },
        ),
        (
            "SessionStatusChanged",
            DaemonMessageType::Session {
                session_id: Some(1),
                event: SessionEvent::SessionStatusChanged {
                    status: SessionStatus::ToolCall("sh".into()),
                    last_modified: 0,
                },
            },
        ),
        (
            "SessionFailed",
            DaemonMessageType::Session {
                session_id: Some(1),
                event: SessionEvent::SessionFailed {
                    operation: "create_session".into(),
                    error: "some failure happened here".into(),
                },
            },
        ),
        (
            "Started",
            DaemonMessageType::Session {
                session_id: Some(1),
                event: SessionEvent::Started {
                    stream_id: 1,
                    turn_id: 1,
                    estimated_prompt_tokens: 100,
                },
            },
        ),
        (
            "ToolCallStarted",
            DaemonMessageType::Session {
                session_id: Some(1),
                event: SessionEvent::ToolCallStarted {
                    stream_id: 1,
                    call_id: "call_1".into(),
                    tool_name: "sh".into(),
                    arguments_json: r#"{"command":"echo hi"}"#.into(),
                    invocation_description: "Running command: `echo hi`.".into(),
                },
            },
        ),
        (
            "ToolCallFinished",
            DaemonMessageType::Session {
                session_id: Some(1),
                event: SessionEvent::ToolCallFinished {
                    stream_id: 1,
                    call_id: "call_1".into(),
                    tool_name: "sh".into(),
                },
            },
        ),
        (
            "ToolResultChunk",
            DaemonMessageType::Session {
                session_id: Some(1),
                event: SessionEvent::ToolResultChunk {
                    stream_id: 1,
                    call_id: "call_1".into(),
                    data: vec![b'x'; 100],
                },
            },
        ),
        (
            "ToolCallFailed",
            DaemonMessageType::Session {
                session_id: Some(1),
                event: SessionEvent::ToolCallFailed {
                    stream_id: 1,
                    call_id: "call_1".into(),
                    tool_name: "sh".into(),
                    error: "command not found".into(),
                },
            },
        ),
        (
            "TokenUsageUpdate",
            DaemonMessageType::Session {
                session_id: Some(1),
                event: SessionEvent::TokenUsageUpdate {
                    token_usage: TokenUsage {
                        input_tokens: 100,
                        output_tokens: 50,
                        total_tokens: 150,
                        ..Default::default()
                    },
                    last_prompt_tokens: Some(100),
                },
            },
        ),
        (
            "LiveOutputTokenCount",
            DaemonMessageType::Session {
                session_id: Some(1),
                event: SessionEvent::LiveOutputTokenCount {
                    stream_id: 1,
                    output_tokens: 42,
                },
            },
        ),
        (
            "OutputChunk",
            DaemonMessageType::Session {
                session_id: Some(1),
                event: SessionEvent::OutputChunk {
                    stream_id: 1,
                    stream: OutputStream::Answer,
                    data: vec![b'x'; 100],
                },
            },
        ),
        (
            "Done",
            DaemonMessageType::Session {
                session_id: Some(1),
                event: SessionEvent::Done {
                    stream_id: 1,
                    token_usage: Some(TokenUsage {
                        input_tokens: 100,
                        output_tokens: 50,
                        total_tokens: 150,
                        ..Default::default()
                    }),
                    last_prompt_tokens: Some(100),
                },
            },
        ),
        (
            "Failed",
            DaemonMessageType::Session {
                session_id: Some(1),
                event: SessionEvent::Failed {
                    stream_id: 1,
                    error: "x".repeat(100),
                },
            },
        ),
        (
            "Cancelled",
            DaemonMessageType::Session {
                session_id: Some(1),
                event: SessionEvent::Cancelled { stream_id: 1 },
            },
        ),
        ("Pong", DaemonMessageType::Pong),
        (
            "Models",
            DaemonMessageType::Models {
                models: vec!["gpt-4".into(), "gpt-4o".into(), "gpt-5.6".into()],
                selected_model: Some("gpt-5.6".into()),
            },
        ),
        (
            "ModelsFailed",
            DaemonMessageType::ModelsFailed {
                error: "failed to list models".into(),
            },
        ),
        (
            "ModelsRefreshed",
            DaemonMessageType::ModelsRefreshed {
                providers: 208,
                models: 1234,
                status: RefreshStatus::Updated,
            },
        ),
        (
            "ModelsRefreshFailed",
            DaemonMessageType::ModelsRefreshFailed {
                error: "network error".into(),
            },
        ),
        (
            "CatalogUpdated",
            DaemonMessageType::CatalogUpdated {
                providers: (0..208)
                    .map(|i| CatalogProvider {
                        slug: format!("provider-slug-{i}"),
                        display_name: format!("Provider Display Name {i}"),
                    })
                    .collect(),
            },
        ),
        (
            "ModelSelected",
            DaemonMessageType::Session {
                session_id: Some(1),
                event: SessionEvent::ModelSelected {
                    model: "gpt-5.6".into(),
                    reasoning_capability: Some(ReasoningCapability {
                        available_effort_levels: vec![
                            "off".into(),
                            "low".into(),
                            "medium".into(),
                            "high".into(),
                        ],
                    }),
                },
            },
        ),
        (
            "ModelSelectionFailed",
            DaemonMessageType::Session {
                session_id: Some(1),
                event: SessionEvent::ModelSelectionFailed {
                    model: "gpt-5.6".into(),
                    error: "model not found".into(),
                },
            },
        ),
        ("Unlocked", DaemonMessageType::Unlocked),
        ("Locked", DaemonMessageType::Locked),
        (
            "Keystore",
            DaemonMessageType::Keystore {
                state: KeystoreState::Unbound,
            },
        ),
        (
            "LockedError",
            DaemonMessageType::LockedError {
                error: "wrong password".into(),
            },
        ),
        (
            "CredentialAdded",
            DaemonMessageType::CredentialAdded {
                service: "openai".into(),
            },
        ),
        (
            "CredentialAddFailed",
            DaemonMessageType::CredentialAddFailed {
                service: "openai".into(),
                error: "already exists".into(),
            },
        ),
        (
            "CredentialRemoved",
            DaemonMessageType::CredentialRemoved {
                service: "openai".into(),
            },
        ),
        (
            "CredentialRemoveFailed",
            DaemonMessageType::CredentialRemoveFailed {
                service: "openai".into(),
                error: "not found".into(),
            },
        ),
        (
            "SessionDeleted",
            DaemonMessageType::Session {
                session_id: Some(1),
                event: SessionEvent::SessionDeleted,
            },
        ),
        (
            "SessionDeleteFailed",
            DaemonMessageType::Session {
                session_id: Some(1),
                event: SessionEvent::SessionDeleteFailed {
                    error: "db error".into(),
                },
            },
        ),
        (
            "TurnsUndone",
            DaemonMessageType::Session {
                session_id: Some(1),
                event: SessionEvent::TurnsUndone {
                    turn_ids: vec![1, 2, 3, 4, 5],
                },
            },
        ),
        (
            "TurnsRedone",
            DaemonMessageType::Session {
                session_id: Some(1),
                event: SessionEvent::TurnsRedone {
                    turns: std::collections::BTreeMap::from([(1, dense_turn), (2, one_turn)]),
                },
            },
        ),
        (
            "Credential",
            DaemonMessageType::Credential {
                service: "openai".into(),
                key: Some("sk-123".into()),
            },
        ),
        (
            "AccountAdded",
            DaemonMessageType::AccountAdded {
                name: "default".into(),
            },
        ),
        (
            "AccountAddFailed",
            DaemonMessageType::AccountAddFailed {
                name: "default".into(),
                error: "invalid provider".into(),
            },
        ),
        (
            "AccountRemoved",
            DaemonMessageType::AccountRemoved {
                name: "default".into(),
            },
        ),
        (
            "AccountRemoveFailed",
            DaemonMessageType::AccountRemoveFailed {
                name: "default".into(),
                error: "not found".into(),
            },
        ),
        (
            "Accounts",
            DaemonMessageType::Accounts {
                accounts: (0..10)
                    .map(|i| AccountInfo {
                        name: format!("account-{i}"),
                        provider: "openai".into(),
                        has_credential: i % 2 == 0,
                    })
                    .collect(),
            },
        ),
        (
            "AccountListFailed",
            DaemonMessageType::AccountListFailed {
                error: "failed to list accounts".into(),
            },
        ),
        (
            "SessionAccountSet",
            DaemonMessageType::Session {
                session_id: Some(1),
                event: SessionEvent::SessionAccountSet {
                    account: "default".into(),
                },
            },
        ),
        (
            "ContextWindowResolved",
            DaemonMessageType::Session {
                session_id: Some(1),
                event: SessionEvent::ContextWindowResolved {
                    context_window: 128_000,
                },
            },
        ),
        (
            "SessionWorkingDirSet",
            DaemonMessageType::Session {
                session_id: Some(1),
                event: SessionEvent::SessionWorkingDirSet {
                    path: Some("/tmp".into()),
                },
            },
        ),
        (
            "SessionTitleSet",
            DaemonMessageType::Session {
                session_id: Some(1),
                event: SessionEvent::SessionTitleSet {
                    title: "hello".into(),
                },
            },
        ),
        (
            "ReasoningEffortSet",
            DaemonMessageType::Session {
                session_id: Some(1),
                event: SessionEvent::ReasoningEffortSet {
                    effort: "high".into(),
                },
            },
        ),
        (
            "ReasoningEffortSetFailed",
            DaemonMessageType::Session {
                session_id: Some(1),
                event: SessionEvent::ReasoningEffortSetFailed {
                    effort: "high".into(),
                    error: "model does not support it".into(),
                },
            },
        ),
        (
            "Image",
            DaemonMessageType::Image {
                session_id: 1,
                turn_id: 1,
                key: ImageKey::Displayed { index: 0 },
                data: Some(vec![0u8; 4096]),
            },
        ),
        (
            "Image (tool-result key)",
            DaemonMessageType::Image {
                session_id: 1,
                turn_id: 1,
                key: ImageKey::ToolResult {
                    call_id: "call_0123456789abcdef0123456789abcdef".into(),
                },
                data: Some(vec![0u8; 4096]),
            },
        ),
        (
            "McpStatus",
            DaemonMessageType::McpStatus {
                servers: (0..12usize)
                    .map(|i| McpServerStatus {
                        slug: format!("server-{i}"),
                        tier: if i % 2 == 0 { "daemon" } else { "project" }.into(),
                        transport: "stdio".into(),
                        target: format!("/usr/local/bin/mcp-server-{i} --flag"),
                        connected: i % 2 == 0,
                        tool_count: i,
                        server_name: Some(format!("server-{i}-name")),
                        server_version: Some("1.2.3".into()),
                        last_error: (i % 2 == 1).then(|| "connect timed out".to_string()),
                    })
                    .collect(),
                project_root: Some("/home/u/work/my-project".into()),
                project_trusted: true,
                ignored_project_servers: vec!["docs".into(), "notes".into()],
            },
        ),
        (
            "McpTrustUpdated",
            DaemonMessageType::McpTrustUpdated {
                root: Some("/home/u/work/my-project".into()),
                trusted: true,
                message: "trusted project MCP root /home/u/work/my-project".into(),
            },
        ),
        (
            "McpTrustList",
            DaemonMessageType::McpTrustList {
                roots: vec![
                    "/home/u/work/my-project".into(),
                    "/home/u/other-project".into(),
                ],
            },
        ),
        (
            "McpReconnectFailed",
            DaemonMessageType::McpReconnectFailed {
                slug: "docs".into(),
                error: "failed to list tools: connection refused".into(),
            },
        ),
        (
            "McpReloaded",
            DaemonMessageType::McpReloaded {
                summary: "MCP reload: 1 added, 0 removed, 1 restarted, 2 unchanged, 0 failed"
                    .into(),
                servers: (0..12usize)
                    .map(|i| McpServerStatus {
                        slug: format!("server-{i}"),
                        tier: if i % 2 == 0 { "daemon" } else { "project" }.into(),
                        transport: "stdio".into(),
                        target: format!("/usr/local/bin/mcp-server-{i} --flag"),
                        connected: i % 2 == 0,
                        tool_count: i,
                        server_name: Some(format!("server-{i}-name")),
                        server_version: Some("1.2.3".into()),
                        last_error: (i % 2 == 1).then(|| "connect timed out".to_string()),
                    })
                    .collect(),
            },
        ),
        (
            "McpReloadFailed",
            DaemonMessageType::McpReloadFailed {
                error: "failed to parse /home/u/.config/choreographr/mcp.json".into(),
            },
        ),
        ("ShuttingDown", DaemonMessageType::ShuttingDown),
        ("Evicted", DaemonMessageType::Evicted),
    ];

    let mut checked = 0usize;
    for (name, inner) in &samples {
        // Every broadcast rides `id: None`; the estimate must cover the
        // broadcast frame (the id field is absent on the wire).
        let msg = DaemonMessage::broadcast(inner.clone());
        let frame = crate::encode_frame(&msg).expect("encode");
        // The 4-byte BE length prefix precedes the payload; the estimate
        // must cover the payload itself.
        let payload = frame.len() - 4;
        let est = msg.approx_wire_size();
        assert!(
            est >= payload,
            "approx_wire_size ({est}) UNDER-estimates the {payload}-byte encoded payload by {} for {name}: {msg:?}",
            payload - est
        );
        checked += 1;
    }
    // Id-BEARING frames: a targeted reply carries the extra `id` field on
    // the wire, so the estimate must cover that too (the envelope
    // allowance, not the per-variant payload, absorbs it). Include the new
    // terminal-acknowledgement variants.
    let replies: Vec<(&str, DaemonMessage)> = vec![
        (
            "SessionsReply",
            DaemonMessage::reply(
                42,
                DaemonMessageType::Sessions {
                    sessions: (0..20).map(summary).collect(),
                },
            ),
        ),
        (
            "SessionStateReply",
            DaemonMessage::reply(
                7,
                DaemonMessageType::Session {
                    session_id: Some(1),
                    event: SessionEvent::SessionState {
                        title: Some("t".into()),
                        selected_model: Some("gpt-5.6".into()),
                        parent_session_id: None,
                        working_dir: Some("/tmp".into()),
                        turns: BTreeMap::new(),
                        active_tool_groups: vec!["core".into()],
                        token_usage: None,
                        context_window: None,
                        last_prompt_tokens: None,
                        status: SessionStatus::Inactive,
                        reasoning_effort: None,
                        reasoning_capability: None,
                    },
                },
            ),
        ),
        (
            "Accepted",
            DaemonMessage::reply(
                9,
                DaemonMessageType::Accepted {
                    kind: MessageKind::SetSessionPinned,
                },
            ),
        ),
        (
            "Failed",
            DaemonMessage::reply(
                11,
                DaemonMessageType::Failed {
                    kind: MessageKind::RunInput,
                    error: "session not attached".into(),
                },
            ),
        ),
    ];
    for (name, msg) in &replies {
        let frame = crate::encode_frame(msg).expect("encode");
        let payload = frame.len() - 4;
        let est = msg.approx_wire_size();
        assert!(
            est >= payload,
            "approx_wire_size ({est}) UNDER-estimates the {payload}-byte encoded payload by {} for {name} (reply): {msg:?}",
            payload - est
        );
        checked += 1;
    }
    assert_eq!(
        checked,
        samples.len() + replies.len(),
        "every sample must be checked"
    );
}
