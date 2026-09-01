//! Lag-eviction byte gauge: cheap, conservative estimates of the serialized
//! wire size of [`Turn`] and [`DaemonMessage`].
//!
//! These estimates are the threshold the daemon's lag accounting
//! (`choreo-daemon::broadcast::SubscriberSink`) compares against the
//! per-client cap / global budget, so they must never UNDER-estimate the
//! encoded payload (a genuinely lagging client could then escape eviction).
//! They deliberately over-estimate: over-counting only makes the lag limit
//! slightly conservative.
//!
//! MAINTENANCE: the per-arm `named_field_overhead(n)` field counts, the
//! per-record allowances in [`Turn::approx_size`], and the fixed
//! [`DaemonMessage::approx_wire_size`] envelope are hand-tuned against the
//! MessagePack named-mode encoder. When a serialized struct/variant gains a
//! field, or a record type's shape changes, update the count/allowance HERE
//! in the same change — the pin test
//! `types::tests::approx_wire_size_never_underestimates_encoded_payload`
//! (in `types.rs`) encodes every variant with dense payloads and fails on
//! any under-estimate, so a drift surfaces as a test failure.

use crate::{
    DaemonMessage, OutputStream, ReasoningArtifact, ReasoningCapability, SessionEvent,
    SessionStatus, Turn,
};

impl Turn {
    /// Cheap, conservative estimate of this turn's serialized byte size.
    ///
    /// Used by [`DaemonMessage::approx_wire_size`] (daemon-side lag
    /// accounting). Sums the variable-size fields — text, tool-call
    /// arguments, tool-result content, and image binary data — plus a fixed
    /// per-record overhead, and deliberately over-estimates: it is a
    /// threshold gauge, so over-counting only makes the lag limit slightly
    /// conservative, while under-counting could let a genuinely lagging
    /// client escape eviction. Fixed-size scalars (timestamps, token counts,
    /// flags) are ignored.
    pub fn approx_size(&self) -> usize {
        // Fixed per-turn overhead from `named_field_overhead(13)` (the 13
        // named-mode field-name keys + tags; an all-None turn serializes to
        // ~200 bytes). The arms below add only payload bytes on top, so the
        // estimate always over-counts: it is a threshold gauge, where
        // over-counting just makes the lag limit slightly conservative.
        let mut size = named_field_overhead(13);
        if let Some(s) = &self.error {
            size += s.len();
        }
        if let Some(s) = &self.user_text {
            size += s.len();
        }
        if let Some(s) = &self.assistant_text {
            size += s.len();
        }
        if let Some(s) = &self.assistant_reasoning {
            size += s.len();
        }
        for call in &self.tool_calls {
            // Real MessagePack named-mode cost per `AssistantToolCallRecord` is
            // ~57 B of overhead (map header + variant tag + the three field-
            // name keys), so a 16 B allowance under-counted tool-heavy turns:
            // the lag gauge must never UNDER-estimate the serialized payload
            // (a genuinely lagging client could then escape eviction). 64 B
            // leaves comfortable slack for longer keys/payload headers.
            size += 64 + call.call_id.len() + call.name.len() + call.arguments_json.len();
        }
        for result in &self.tool_results {
            // Real per-`ToolResultRecord` overhead is ~79 B (five field keys,
            // the longest being `invocation_description` at 21 chars); the
            // old 32 B allowance under-counted by ~47 B per result. 96 B
            // over-covers it, keeping the gauge conservative.
            size += 96
                + result.call_id.len()
                + result.name.len()
                + result.content.len()
                + result.invocation_description.len();
        }
        for image in &self.displayed_images {
            // Real per-`DisplayedImageRecord` overhead is ~127 B (the nested
            // `ImageMetadata` struct adds its own tag + five field keys); the
            // old 48 B allowance under-counted by ~79 B per image. 160 B
            // over-covers it.
            size += 160
                + image.data.len()
                + image.metadata.mime_type.len()
                + image.metadata.alt.as_ref().map_or(0, String::len);
        }
        if let Some(artifact) = &self.reasoning_artifact {
            // Opaque round-trip payload: count the raw bytes, tagged by the
            // variant that owns them. Real overhead is ~40 B for the bare-byte
            // variants but ~88 B for `ChatReasoning` (an extra nested map +
            // the `field` enum tag), so 96 B covers every variant with slack.
            let payload = match artifact {
                ReasoningArtifact::ChatReasoning { bytes, .. } => bytes.len(),
                ReasoningArtifact::AnthropicThinking(bytes)
                | ReasoningArtifact::GoogleSignatures(bytes)
                | ReasoningArtifact::ResponsesItems(bytes) => bytes.len(),
            };
            size += 96 + payload;
        }
        if let Some(producer) = &self.reasoning_producer {
            // Real per-`ReasoningProducer` overhead is ~60 B (its own variant
            // tag + two field keys); the old 16 B allowance under-counted.
            size += 64 + producer.provider_slug.len() + producer.model.len();
        }
        size
    }
}

/// Byte length of an `Option<String>`'s payload (0 when None). Shared by
/// [`DaemonMessage::approx_wire_size`] so every optional string field is
/// counted the same way. Only the payload is counted here — the MessagePack
/// field-name key and length prefix are covered by the per-field allowance of
/// [`named_field_overhead`], which the arm's fixed base provides.
fn option_str_len(s: &Option<String>) -> usize {
    s.as_ref().map_or(0, String::len)
}

/// Estimated MessagePack named-mode overhead for a struct variant with `n`
/// fields: the variant tag, map headers, and one field-name key per field.
/// The allowance is ~24 bytes per field (the longest field-name keys in use
/// are ~21 chars plus a length prefix; the value side is a nil marker, a
/// scalar, or a string header — all covered here) plus a fixed tag/header
/// allowance, so the base alone covers every field's key + value tag whether
/// the value is absent or present. Arms then add only the variable PAYLOAD
/// bytes on top. Over-estimating a little is the conservative direction for
/// a lag-eviction gauge: under-counting could let a lagging client escape
/// eviction.
fn named_field_overhead(fields: usize) -> usize {
    40 + fields * 24
}

/// Byte length of a `SessionStatus` payload (a small enum; only the
/// `ToolCall(String)` and `Retrying` variants carry data).
fn session_status_size(status: &SessionStatus) -> usize {
    match status {
        SessionStatus::ToolCall(name) => 8 + name.len(),
        SessionStatus::Retrying { .. } => 16,
        SessionStatus::Sleeping | SessionStatus::Inactive | SessionStatus::Inference => 4,
    }
}

/// Byte length of a `ReasoningCapability` payload: the named-mode variant
/// tag, map header, `available_effort_levels` field key, the array header,
/// and one string header per effort level, plus the level slugs' payloads.
/// Fixed at ~47 B for a 4-level set, so the 64 B fixed part over-covers it
/// and the per-element +1 accounts for longer level lists.
fn reasoning_capability_size(cap: &ReasoningCapability) -> usize {
    64 + cap.available_effort_levels.len()
        + cap
            .available_effort_levels
            .iter()
            .map(String::len)
            .sum::<usize>()
}

/// Byte length of a session-scoped [`SessionEvent`]'s serialized payload.
///
/// Mirrors the pre-split `DaemonMessage::approx_wire_size` arms for the 29
/// session-scoped variants, minus the `session_id` field that now lives on
/// the [`DaemonMessage::Session`] envelope (the envelope's own 2-field
/// overhead is added by the `Session` arm). Same accounting style: per-field
/// named-mode key overhead via [`named_field_overhead`], variable-size
/// strings/byte buffers/vecs/turn maps summed, fixed-size scalars (u32/u64
/// ids, token counts, status enums without payload) ignored, and the generous
/// fixed [`OVERHEAD`] for fieldless variants.
fn session_event_size(event: &SessionEvent) -> usize {
    const OVERHEAD: usize = 96;
    match event {
        SessionEvent::SessionCreated {
            title,
            working_dir,
            account_name,
            selected_model,
            reasoning_effort,
            ..
        } => {
            named_field_overhead(6)
                + option_str_len(title)
                + option_str_len(working_dir)
                + option_str_len(account_name)
                + option_str_len(selected_model)
                + option_str_len(reasoning_effort)
        }
        SessionEvent::SessionAttached | SessionEvent::SessionDeleted => OVERHEAD,
        SessionEvent::SessionState {
            title,
            selected_model,
            working_dir,
            turns,
            active_tool_groups,
            reasoning_effort,
            status,
            reasoning_capability,
            ..
        } => {
            named_field_overhead(12)
                + option_str_len(title)
                + option_str_len(selected_model)
                + option_str_len(working_dir)
                + option_str_len(reasoning_effort)
                + active_tool_groups.iter().map(String::len).sum::<usize>()
                + turns
                    .iter()
                    .map(|(id, turn)| 8 + *id as usize + turn.approx_size())
                    .sum::<usize>()
                + session_status_size(status)
                + reasoning_capability
                    .as_ref()
                    .map_or(0, reasoning_capability_size)
        }
        SessionEvent::TurnAppended { turn, .. } => named_field_overhead(2) + turn.approx_size(),
        SessionEvent::SessionStatusChanged { status, .. } => {
            named_field_overhead(2) + session_status_size(status)
        }
        SessionEvent::SessionFailed { operation, error } => {
            named_field_overhead(2) + operation.len() + error.len()
        }
        SessionEvent::Started { .. } => named_field_overhead(3),
        SessionEvent::ToolCallStarted {
            call_id,
            tool_name,
            arguments_json,
            invocation_description,
            ..
        } => {
            named_field_overhead(5)
                + call_id.len()
                + tool_name.len()
                + arguments_json.len()
                + invocation_description.len()
        }
        SessionEvent::ToolCallFinished {
            call_id, tool_name, ..
        } => named_field_overhead(3) + call_id.len() + tool_name.len(),
        SessionEvent::ToolResultChunk { call_id, data, .. } => {
            named_field_overhead(3) + call_id.len() + data.len()
        }
        SessionEvent::ToolCallFailed {
            call_id,
            tool_name,
            error,
            ..
        } => named_field_overhead(4) + call_id.len() + tool_name.len() + error.len(),
        SessionEvent::TokenUsageUpdate { .. } => named_field_overhead(2),
        SessionEvent::LiveOutputTokenCount { .. } => named_field_overhead(2),
        SessionEvent::OutputChunk { stream, data, .. } => {
            let stream_len = match stream {
                OutputStream::Answer | OutputStream::Reasoning => 4,
            };
            named_field_overhead(3) + stream_len + data.len()
        }
        SessionEvent::Done { .. } => named_field_overhead(3),
        SessionEvent::Failed { error, .. } => named_field_overhead(2) + error.len(),
        SessionEvent::Cancelled { .. } => named_field_overhead(1),
        SessionEvent::ModelSelected {
            model,
            reasoning_capability,
            ..
        } => {
            named_field_overhead(2)
                + model.len()
                + reasoning_capability
                    .as_ref()
                    .map_or(0, reasoning_capability_size)
        }
        SessionEvent::ModelSelectionFailed { model, error, .. } => {
            named_field_overhead(2) + model.len() + error.len()
        }
        SessionEvent::SessionDeleteFailed { error } => named_field_overhead(1) + error.len(),
        SessionEvent::TurnsUndone { turn_ids } => named_field_overhead(1) + turn_ids.len() * 4,
        SessionEvent::TurnsRedone { turns } => {
            named_field_overhead(1)
                + turns
                    .iter()
                    .map(|(id, turn)| 8 + *id as usize + turn.approx_size())
                    .sum::<usize>()
        }
        SessionEvent::SessionAccountSet { account } => named_field_overhead(1) + account.len(),
        SessionEvent::ContextWindowResolved { .. } => named_field_overhead(1),
        SessionEvent::SessionWorkingDirSet { path } => {
            named_field_overhead(1) + option_str_len(path)
        }
        SessionEvent::SessionTitleSet { title } => named_field_overhead(1) + title.len(),
        SessionEvent::ReasoningEffortSet { effort } => named_field_overhead(1) + effort.len(),
        SessionEvent::ReasoningEffortSetFailed { effort, error } => {
            named_field_overhead(2) + effort.len() + error.len()
        }
    }
}

impl DaemonMessage {
    /// Cheap, conservative estimate of this message's serialized byte size.
    ///
    /// Used by the daemon's lag accounting (broadcast::SubscriberSink) to
    /// gauge how many bytes a slow client has fallen behind the streaming
    /// frontier. It
    /// deliberately over-estimates rather than serializes: the accounting is
    /// a threshold gauge, so a slightly-too-big estimate can only trigger
    /// eviction a little early — never let a genuinely lagging client slip
    /// past the limit. O(1) in message count — it sums the variable-size
    /// fields (strings, byte buffers, vecs, turn maps) plus a fixed
    /// per-variant envelope overhead, and ignores fixed-size scalars.
    /// The over-estimate is pinned by the
    /// `approx_wire_size_never_underestimates_encoded_payload` test in `types.rs`,
    /// which encodes every `DaemonMessage` variant with realistic payloads and
    /// asserts the estimate covers the actual bytes.
    pub fn approx_wire_size(&self) -> usize {
        // Fixed envelope overhead for FIELDLESS variants (Pong, Evicted, …):
        // just the variant tag, so this generous fixed value is a comfortable
        // over-estimate. Struct variants get their per-field named-mode key
        // overhead from `named_field_overhead` instead.
        const OVERHEAD: usize = 96;
        match self {
            // Session-scoped events ride the `Session` envelope: fixed
            // 2-field envelope overhead (variant tag + map header + the
            // `session_id`/`event` field keys) plus the inner event's own
            // per-field estimate. The `Option<u64>` `session_id` payload
            // (1 byte for nil, up to ~9 for `Some(u64)`) is comfortably
            // covered by the fixed 88-byte envelope allowance (40 + 2×24),
            // so both encodings fit without per-arm accounting.
            Self::Session { event, .. } => named_field_overhead(2) + session_event_size(event),
            Self::Sessions { sessions } => {
                named_field_overhead(1)
                    + sessions
                        .iter()
                        .map(|s| {
                            // 15-field SessionSummary: named-mode keys + tags
                            // + status string (the variable-size status
                            // payload is not otherwise counted).
                            named_field_overhead(15)
                                + option_str_len(&s.title)
                                + option_str_len(&s.selected_model)
                                + option_str_len(&s.reasoning_effort)
                                + option_str_len(&s.working_dir)
                                + option_str_len(&s.account_name)
                                + s.active_tool_groups.iter().map(String::len).sum::<usize>()
                                + session_status_size(&s.status)
                        })
                        .sum::<usize>()
            }
            Self::Pong => OVERHEAD,
            Self::Models {
                models,
                selected_model,
            } => {
                named_field_overhead(2)
                    + models.iter().map(String::len).sum::<usize>()
                    + option_str_len(selected_model)
            }
            Self::ModelsFailed { error } => named_field_overhead(1) + error.len(),
            Self::Unlocked | Self::Locked | Self::ShuttingDown | Self::Evicted => OVERHEAD,
            Self::LockedError { error } => named_field_overhead(1) + error.len(),
            Self::CredentialAdded { service } => named_field_overhead(1) + service.len(),
            Self::CredentialAddFailed { service, error } => {
                named_field_overhead(2) + service.len() + error.len()
            }
            Self::CredentialRemoved { service } => named_field_overhead(1) + service.len(),
            Self::CredentialRemoveFailed { service, error } => {
                named_field_overhead(2) + service.len() + error.len()
            }
            Self::AclAddResult { ok: _, message } => named_field_overhead(2) + message.len(),
            Self::AclUpdated { .. } => named_field_overhead(1),
            Self::Credential { service, key } => {
                named_field_overhead(2) + service.len() + option_str_len(key)
            }
            Self::AccountAdded { name } => named_field_overhead(1) + name.len(),
            Self::AccountAddFailed { name, error } => {
                named_field_overhead(2) + name.len() + error.len()
            }
            Self::AccountRemoved { name } => named_field_overhead(1) + name.len(),
            Self::AccountRemoveFailed { name, error } => {
                named_field_overhead(2) + name.len() + error.len()
            }
            Self::Accounts { accounts } => {
                named_field_overhead(1)
                    + accounts
                        .iter()
                        .map(|a| 48 + a.name.len() + a.provider.len())
                        .sum::<usize>()
            }
            Self::AccountListFailed { error } => named_field_overhead(1) + error.len(),
            // providers/models are usize COUNTS (fixed-size scalars), covered
            // by the per-field allowance.
            Self::ModelsRefreshed { .. } => named_field_overhead(3),
            Self::ModelsRefreshFailed { error } => named_field_overhead(1) + error.len(),
            Self::CatalogUpdated { providers } => {
                named_field_overhead(1)
                    + providers
                        .iter()
                        .map(|p| 32 + p.slug.len() + p.display_name.len())
                        .sum::<usize>()
            }
        }
    }
}
